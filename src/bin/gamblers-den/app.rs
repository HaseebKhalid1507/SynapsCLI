use std::time::Duration;
use crossterm::event::KeyEvent;
use crate::save::SaveData;
use crate::games::blackjack::{BlackjackGame, GamePhase, Outcome};
use crate::games::slots::{SlotsGame, SlotsPhase};
use crate::games::roulette::{RouletteGame, RoulettePhase, BET_OPTIONS as ROULETTE_BETS};
use crate::games::war::{WarGame, WarPhase};
use crate::games::baccarat::{BaccaratGame, BaccaratPhase, BaccaratBet};
use crate::games::video_poker::{VideoPokerGame, VideoPokerPhase};
use crate::games::keno::{KenoGame, KenoPhase};
use crate::games::sicbo::{SicBoGame, SicBoPhase, BET_OPTIONS as SICBO_BETS};
use crate::games::craps::{CrapsGame, CrapsPhase, CrapsBet};

/// All possible screens in the game.
#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    Boot,
    Hub,
    Blackjack,
    Slots,
    Roulette,
    War,
    Baccarat,
    VideoPoker,
    Keno,
    SicBo,
    Craps,
    GameOver,
}

/// Root application state.
pub struct App {
    pub screen: Screen,
    pub tokens: u64,
    pub should_quit: bool,
    pub save: SaveData,
    pub elapsed: Duration,
    pub frame: u64,
    pub hub_selection: usize,
    pub blackjack: BlackjackGame,
    pub slots: SlotsGame,
    pub roulette: RouletteGame,
    pub war: WarGame,
    pub baccarat: BaccaratGame,
    pub video_poker: VideoPokerGame,
    pub keno: KenoGame,
    pub sicbo: SicBoGame,
    pub craps: CrapsGame,
    pub transition: crate::ui::transition::Transition,
}

impl App {
    pub fn new() -> Self {
        let save = crate::save::load();
        let tokens = save.tokens;
        Self {
            screen: Screen::Boot,
            tokens,
            should_quit: false,
            save,
            elapsed: Duration::ZERO,
            frame: 0,
            hub_selection: 0,
            blackjack: BlackjackGame::new(),
            slots: SlotsGame::new(),
            roulette: RouletteGame::new(),
            war: WarGame::new(),
            baccarat: BaccaratGame::new(),
            video_poker: VideoPokerGame::new(),
            keno: KenoGame::new(),
            sicbo: SicBoGame::new(),
            craps: CrapsGame::new(),
            transition: crate::ui::transition::Transition::new(),
        }
    }

    pub fn tick(&mut self, dt: Duration) {
        self.elapsed += dt;
        self.frame = self.frame.wrapping_add(1);
        self.transition.tick();

        // Boot auto-advance
        if self.screen == Screen::Boot && self.elapsed.as_millis() > 2500 {
            self.switch_screen(Screen::Hub);
        }

        // Blackjack animation ticks
        if self.screen == Screen::Blackjack {
            self.blackjack.phase_timer += 1;

            match &self.blackjack.phase {
                GamePhase::Dealing => {
                    // Animate card dealing — reveal one card every 15 frames
                    let target = self.blackjack.player_hand.len() + self.blackjack.dealer_hand.len();
                    self.blackjack.visible_cards = (self.blackjack.phase_timer as usize / 15).min(target);
                    if self.blackjack.visible_cards >= target {
                        self.blackjack.after_deal();
                    }
                }
                GamePhase::DealerTurn => {
                    // Dealer draws one card every 30 frames
                    if self.blackjack.phase_timer % 30 == 0 {
                        let done = self.blackjack.dealer_step();
                        if done {
                            let payout = self.blackjack.resolve();
                            self.apply_payout(payout);
                        }
                    }
                }
                _ => {}
            }
        }

        // Slots animation ticks
        if self.screen == Screen::Slots {
            match &self.slots.phase {
                SlotsPhase::Spinning | SlotsPhase::Revealing(_) => {
                    let done = self.slots.tick_spin();
                    if done {
                        self.apply_slots_payout();
                    }
                }
                _ => {}
            }
        }

        // Roulette animation ticks
        if self.screen == Screen::Roulette {
            if self.roulette.phase == RoulettePhase::Spinning {
                let done = self.roulette.tick_spin();
                if done {
                    self.apply_roulette_payout();
                }
            }
        }

        // War animation ticks
        if self.screen == Screen::War {
            self.war.phase_timer += 1;
            if self.war.phase == WarPhase::Reveal && self.war.phase_timer > 30 {
                self.war.resolve_reveal();
                if self.war.phase == WarPhase::Result {
                    self.apply_generic_payout(self.war.last_payout);
                }
            }
            if self.war.phase == WarPhase::War && self.war.phase_timer > 40 {
                self.war.go_to_war();
            }
            if self.war.phase == WarPhase::WarReveal && self.war.phase_timer > 30 {
                self.war.resolve_war();
                self.apply_generic_payout(self.war.last_payout);
            }
        }

        // Baccarat animation ticks
        if self.screen == Screen::Baccarat {
            self.baccarat.phase_timer += 1;
            if self.baccarat.phase == BaccaratPhase::Dealing && self.baccarat.phase_timer > 40 {
                self.baccarat.resolve();
                self.apply_generic_payout(self.baccarat.last_payout);
            }
        }

        // Keno animation ticks
        if self.screen == Screen::Keno {
            if self.keno.phase == KenoPhase::Drawing {
                self.keno.phase_timer += 1;
                if self.keno.phase_timer % 8 == 0 {
                    let done = self.keno.draw_one();
                    if done {
                        self.keno.resolve();
                        self.apply_generic_payout(self.keno.last_payout);
                    }
                }
            }
        }

        // Sic Bo animation ticks
        if self.screen == Screen::SicBo {
            if self.sicbo.phase == SicBoPhase::Rolling {
                let done = self.sicbo.tick_roll();
                if done {
                    self.sicbo.resolve();
                    self.apply_generic_payout(self.sicbo.last_payout);
                }
            }
        }

        // Craps animation ticks
        if self.screen == Screen::Craps {
            if matches!(self.craps.phase, CrapsPhase::ComeOut | CrapsPhase::Point) {
                if self.craps.phase_timer < 40 {
                    self.craps.tick_roll();
                } else if self.craps.phase_timer == 40 {
                    self.craps.rolling_display = self.craps.dice;
                    if let Some(payout) = self.craps.resolve_roll() {
                        self.apply_generic_payout(payout);
                    }
                }
                self.craps.phase_timer += 1;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyCode;

        match &self.screen {
            Screen::Hub => match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.hub_selection = self.hub_selection.saturating_sub(1);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    self.hub_selection = (self.hub_selection + 1).min(8);
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.enter_selected_game();
                }
                KeyCode::Char('1') | KeyCode::Char('b') => {
                    self.hub_selection = 0;
                    self.enter_blackjack();
                }
                KeyCode::Char('2') | KeyCode::Char('s') => {
                    self.hub_selection = 1;
                    self.slots.new_spin();
                    self.screen = Screen::Slots;
                }
                KeyCode::Char('3') | KeyCode::Char('r') => {
                    self.hub_selection = 2;
                    self.roulette.new_round();
                    self.screen = Screen::Roulette;
                }
                _ => {}
            },

            Screen::Blackjack => self.handle_blackjack_key(key),
            Screen::Slots => self.handle_slots_key(key),
            Screen::Roulette => self.handle_roulette_key(key),
            Screen::War => self.handle_simple_card_game_key(key, "war"),
            Screen::Baccarat => self.handle_simple_card_game_key(key, "baccarat"),
            Screen::VideoPoker => self.handle_video_poker_key(key),
            Screen::Keno => self.handle_keno_key(key),
            Screen::SicBo => self.handle_simple_dice_game_key(key, "sicbo"),
            Screen::Craps => self.handle_craps_key(key),

            Screen::Boot => {
                self.switch_screen(Screen::Hub);
            }
            Screen::GameOver => {
                crate::save::reset(&mut self.save);
                self.tokens = self.save.tokens;
                let _ = crate::save::save(&self.save);
                self.switch_screen(Screen::Hub);
            }
        }
    }

    fn handle_blackjack_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyCode;

        match &self.blackjack.phase {
            GamePhase::Betting => match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.blackjack.bet_input.push(c);
                }
                KeyCode::Backspace => {
                    self.blackjack.bet_input.pop();
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    // All-in
                    self.blackjack.bet_input = self.tokens.to_string();
                }
                KeyCode::Enter => {
                    if let Ok(bet) = self.blackjack.bet_input.parse::<u64>() {
                        if bet > 0 && bet <= self.tokens {
                            self.blackjack.deal(bet);
                        }
                    }
                }
                KeyCode::Esc => {
                    self.blackjack.new_hand();
                    self.switch_screen(Screen::Hub);
                }
                _ => {}
            },
            GamePhase::PlayerTurn => match key.code {
                KeyCode::Char('h') | KeyCode::Char('H') => {
                    self.blackjack.hit();
                    if let GamePhase::Result(ref outcome) = self.blackjack.phase {
                        let payout = self.blackjack.calculate_payout(outcome);
                        self.blackjack.last_payout = payout;
                        self.apply_payout(payout);
                    }
                }
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    self.blackjack.stand();
                }
                KeyCode::Char('d') | KeyCode::Char('D') => {
                    if self.blackjack.player_hand.len() == 2 && self.blackjack.bet <= self.tokens {
                        // Need enough tokens for the double
                        let extra = self.blackjack.bet; // will be doubled
                        if extra <= self.tokens - self.blackjack.bet {
                            self.blackjack.double_down();
                            if let GamePhase::Result(ref outcome) = self.blackjack.phase {
                                let payout = self.blackjack.calculate_payout(outcome);
                                self.blackjack.last_payout = payout;
                                self.apply_payout(payout);
                            }
                        }
                    }
                }
                KeyCode::Esc => {
                    // Can't quit mid-hand, but we could add surrender later
                }
                _ => {}
            },
            GamePhase::Result(_) => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.blackjack.new_hand();
                }
                KeyCode::Esc => {
                    self.blackjack.new_hand();
                    self.switch_screen(Screen::Hub);
                }
                _ => {}
            },
            // During animations, no input
            _ => {}
        }
    }

    fn handle_slots_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyCode;

        match &self.slots.phase {
            SlotsPhase::Betting => match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.slots.bet_input.push(c);
                }
                KeyCode::Backspace => {
                    self.slots.bet_input.pop();
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.slots.bet_input = self.tokens.to_string();
                }
                KeyCode::Enter => {
                    if let Ok(bet) = self.slots.bet_input.parse::<u64>() {
                        if bet > 0 && bet <= self.tokens {
                            self.slots.spin(bet);
                        }
                    }
                }
                KeyCode::Esc => {
                    self.slots.new_spin();
                    self.switch_screen(Screen::Hub);
                }
                _ => {}
            },
            SlotsPhase::Spinning | SlotsPhase::Revealing(_) => {
                // No input during spin animation
            }
            SlotsPhase::Result => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.slots.new_spin();
                }
                KeyCode::Esc => {
                    self.slots.new_spin();
                    self.switch_screen(Screen::Hub);
                }
                _ => {}
            },
        }
    }

    fn handle_roulette_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyCode;

        match &self.roulette.phase {
            RoulettePhase::Betting => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.roulette.cursor = self.roulette.cursor.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.roulette.cursor = (self.roulette.cursor + 1).min(ROULETTE_BETS.len() - 1);
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.roulette.bet_input.push(c);
                }
                KeyCode::Backspace => {
                    self.roulette.bet_input.pop();
                }
                KeyCode::Enter => {
                    // Place bet on selected option
                    if let Ok(amount) = self.roulette.bet_input.parse::<u64>() {
                        if amount > 0 && self.roulette.total_bet + amount <= self.tokens {
                            let bet_type = ROULETTE_BETS[self.roulette.cursor].clone();
                            self.roulette.place_bet(bet_type, amount);
                            self.roulette.bet_input.clear();
                        }
                    }
                }
                KeyCode::Char(' ') => {
                    // Spin the wheel
                    if !self.roulette.bets.is_empty() {
                        self.roulette.spin();
                    }
                }
                KeyCode::Esc => {
                    if self.roulette.bets.is_empty() {
                        self.roulette.new_round();
                        self.switch_screen(Screen::Hub);
                    } else {
                        // Clear bets instead of leaving
                        self.roulette.new_round();
                    }
                }
                _ => {}
            },
            RoulettePhase::Spinning => {
                // No input during spin
            }
            RoulettePhase::Result => match key.code {
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.roulette.new_round();
                }
                KeyCode::Esc => {
                    self.roulette.new_round();
                    self.switch_screen(Screen::Hub);
                }
                _ => {}
            },
        }
    }

    fn apply_roulette_payout(&mut self) {
        let payout = self.roulette.calculate_payout();
        self.roulette.last_payout = payout;

        if payout > 0 {
            self.tokens += payout as u64;
            self.save.total_earned += payout as u64;
            self.save.roulette.won += 1;
            if payout as u64 > self.save.roulette.biggest_win {
                self.save.roulette.biggest_win = payout as u64;
            }
        } else if payout < 0 {
            let loss = (-payout) as u64;
            self.tokens = self.tokens.saturating_sub(loss);
            self.save.total_lost += loss;
            self.save.roulette.lost += 1;
            if loss > self.save.roulette.biggest_loss {
                self.save.roulette.biggest_loss = loss;
            }
        }

        self.save.roulette.played += 1;
        self.save.games_played += 1;
        self.persist();
        self.check_game_over();
    }

    fn apply_slots_payout(&mut self) {
        let payout = self.slots.last_payout;
        if payout > 0 {
            self.tokens += payout as u64;
            self.save.total_earned += payout as u64;
            self.save.slots.base.won += 1;
            if payout as u64 > self.save.slots.base.biggest_win {
                self.save.slots.base.biggest_win = payout as u64;
            }
            if self.slots.multiplier >= 50.0 {
                self.save.slots.jackpots += 1;
            }
        } else if payout < 0 {
            let loss = (-payout) as u64;
            self.tokens = self.tokens.saturating_sub(loss);
            self.save.total_lost += loss;
            self.save.slots.base.lost += 1;
            if loss > self.save.slots.base.biggest_loss {
                self.save.slots.base.biggest_loss = loss;
            }
        }
        self.save.slots.base.played += 1;
        self.save.games_played += 1;
        self.persist();
        self.check_game_over();
    }

    fn apply_payout(&mut self, payout: i64) {
        if payout > 0 {
            self.tokens += payout as u64;
            self.save.total_earned += payout as u64;
            self.save.blackjack.base.won += 1;
            if payout as u64 > self.save.blackjack.base.biggest_win {
                self.save.blackjack.base.biggest_win = payout as u64;
            }
        } else if payout < 0 {
            let loss = (-payout) as u64;
            self.tokens = self.tokens.saturating_sub(loss);
            self.save.total_lost += loss;
            self.save.blackjack.base.lost += 1;
            if loss > self.save.blackjack.base.biggest_loss {
                self.save.blackjack.base.biggest_loss = loss;
            }
        } else {
            self.save.blackjack.pushes += 1;
        }
        self.save.blackjack.base.played += 1;
        self.save.games_played += 1;

        // Check for blackjack
        if matches!(self.blackjack.phase, GamePhase::Result(Outcome::PlayerBlackjack)) {
            self.save.blackjack.blackjacks += 1;
        }

        self.persist();
        self.check_game_over();
    }

    fn enter_selected_game(&mut self) {
        match self.hub_selection {
            0 => self.enter_blackjack(),
            1 => { self.slots.new_spin(); self.switch_screen(Screen::Slots); }
            2 => { self.roulette.new_round(); self.switch_screen(Screen::Roulette); }
            3 => { self.war.new_hand(); self.switch_screen(Screen::War); }
            4 => { self.baccarat.new_hand(); self.switch_screen(Screen::Baccarat); }
            5 => { self.video_poker.new_hand(); self.switch_screen(Screen::VideoPoker); }
            6 => { self.keno.new_round(); self.switch_screen(Screen::Keno); }
            7 => { self.sicbo.new_round(); self.switch_screen(Screen::SicBo); }
            8 => { self.craps.new_round(); self.switch_screen(Screen::Craps); }
            _ => {}
        }
    }

    fn enter_blackjack(&mut self) {
        self.blackjack.new_hand();
        self.switch_screen(Screen::Blackjack);
    }

    fn switch_screen(&mut self, screen: Screen) {
        self.transition.trigger();
        self.screen = screen;
    }

    fn handle_simple_card_game_key(&mut self, key: KeyEvent, game: &str) {
        use crossterm::event::KeyCode;
        let is_betting = match game {
            "war" => self.war.phase == WarPhase::Betting,
            "baccarat" => self.baccarat.phase == BaccaratPhase::Betting,
            _ => false,
        };
        let is_result = match game {
            "war" => self.war.phase == WarPhase::Result,
            "baccarat" => self.baccarat.phase == BaccaratPhase::Result,
            _ => false,
        };

        if is_betting {
            match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    match game {
                        "war" => self.war.bet_input.push(c),
                        "baccarat" => self.baccarat.bet_input.push(c),
                        _ => {}
                    }
                }
                KeyCode::Backspace => {
                    match game {
                        "war" => { self.war.bet_input.pop(); },
                        "baccarat" => { self.baccarat.bet_input.pop(); },
                        _ => {}
                    }
                }
                KeyCode::Up | KeyCode::Down if game == "baccarat" => {
                    self.baccarat.cursor = match key.code {
                        KeyCode::Up => self.baccarat.cursor.saturating_sub(1),
                        _ => (self.baccarat.cursor + 1).min(2),
                    };
                }
                KeyCode::Enter => {
                    match game {
                        "war" => {
                            if let Ok(bet) = self.war.bet_input.parse::<u64>() {
                                if bet > 0 && bet <= self.tokens { self.war.deal(bet); }
                            }
                        }
                        "baccarat" => {
                            if let Ok(bet) = self.baccarat.bet_input.parse::<u64>() {
                                if bet > 0 && bet <= self.tokens {
                                    let bt = match self.baccarat.cursor {
                                        0 => BaccaratBet::Player,
                                        1 => BaccaratBet::Banker,
                                        _ => BaccaratBet::Tie,
                                    };
                                    self.baccarat.deal(bet, bt);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            }
        } else if is_result {
            match key.code {
                KeyCode::Enter => {
                    match game {
                        "war" => self.war.new_hand(),
                        "baccarat" => self.baccarat.new_hand(),
                        _ => {}
                    }
                }
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            }
        }
    }

    fn handle_video_poker_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyCode;
        match &self.video_poker.phase {
            VideoPokerPhase::Betting => match key.code {
                KeyCode::Char(c) if c.is_ascii_digit() => self.video_poker.bet_input.push(c),
                KeyCode::Backspace => { self.video_poker.bet_input.pop(); },
                KeyCode::Enter => {
                    if let Ok(bet) = self.video_poker.bet_input.parse::<u64>() {
                        if bet > 0 && bet <= self.tokens { self.video_poker.deal(bet); }
                    }
                }
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            },
            VideoPokerPhase::Hold => match key.code {
                KeyCode::Left => self.video_poker.cursor = self.video_poker.cursor.saturating_sub(1),
                KeyCode::Right => self.video_poker.cursor = (self.video_poker.cursor + 1).min(4),
                KeyCode::Char(' ') | KeyCode::Up => self.video_poker.toggle_hold(self.video_poker.cursor),
                KeyCode::Enter => {
                    self.video_poker.draw_cards();
                    self.apply_generic_payout(self.video_poker.last_payout);
                }
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            },
            VideoPokerPhase::Result => match key.code {
                KeyCode::Enter => self.video_poker.new_hand(),
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_keno_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyCode;
        match &self.keno.phase {
            KenoPhase::Picking => match key.code {
                KeyCode::Left => self.keno.cursor = if self.keno.cursor <= 1 { 80 } else { self.keno.cursor - 1 },
                KeyCode::Right => self.keno.cursor = if self.keno.cursor >= 80 { 1 } else { self.keno.cursor + 1 },
                KeyCode::Up => self.keno.cursor = if self.keno.cursor <= 10 { self.keno.cursor + 70 } else { self.keno.cursor - 10 },
                KeyCode::Down => self.keno.cursor = if self.keno.cursor > 70 { self.keno.cursor - 70 } else { self.keno.cursor + 10 },
                KeyCode::Char(' ') => self.keno.toggle_pick(self.keno.cursor),
                KeyCode::Char(c) if c.is_ascii_digit() => self.keno.bet_input.push(c),
                KeyCode::Backspace => { self.keno.bet_input.pop(); },
                KeyCode::Enter => {
                    if !self.keno.picks.is_empty() {
                        if let Ok(bet) = self.keno.bet_input.parse::<u64>() {
                            if bet > 0 && bet <= self.tokens { self.keno.start_draw(bet); }
                        }
                    }
                }
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            },
            KenoPhase::Drawing => {} // no input during draw
            KenoPhase::Result => match key.code {
                KeyCode::Enter => self.keno.new_round(),
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            },
        }
    }

    fn handle_simple_dice_game_key(&mut self, key: KeyEvent, game: &str) {
        use crossterm::event::KeyCode;
        if game == "sicbo" {
            match &self.sicbo.phase {
                SicBoPhase::Betting => match key.code {
                    KeyCode::Up => self.sicbo.cursor = self.sicbo.cursor.saturating_sub(1),
                    KeyCode::Down => self.sicbo.cursor = (self.sicbo.cursor + 1).min(SICBO_BETS.len() - 1),
                    KeyCode::Char(c) if c.is_ascii_digit() => self.sicbo.bet_input.push(c),
                    KeyCode::Backspace => { self.sicbo.bet_input.pop(); },
                    KeyCode::Enter => {
                        if let Ok(amt) = self.sicbo.bet_input.parse::<u64>() {
                            if amt > 0 && self.sicbo.total_bet + amt <= self.tokens {
                                let bt = SICBO_BETS[self.sicbo.cursor].clone();
                                self.sicbo.place_bet(bt, amt);
                                self.sicbo.bet_input.clear();
                            }
                        }
                    }
                    KeyCode::Char(' ') => {
                        if !self.sicbo.bets.is_empty() { self.sicbo.roll(); }
                    }
                    KeyCode::Esc => {
                        if self.sicbo.bets.is_empty() { self.switch_screen(Screen::Hub); }
                        else { self.sicbo.new_round(); }
                    }
                    _ => {}
                },
                SicBoPhase::Rolling => {}
                SicBoPhase::Result => match key.code {
                    KeyCode::Enter => self.sicbo.new_round(),
                    KeyCode::Esc => self.switch_screen(Screen::Hub),
                    _ => {}
                },
            }
        }
    }

    fn handle_craps_key(&mut self, key: KeyEvent) {
        use crossterm::event::KeyCode;
        match &self.craps.phase {
            CrapsPhase::Betting => match key.code {
                KeyCode::Up => self.craps.cursor = self.craps.cursor.saturating_sub(1),
                KeyCode::Down => self.craps.cursor = (self.craps.cursor + 1).min(2),
                KeyCode::Char(c) if c.is_ascii_digit() => self.craps.bet_input.push(c),
                KeyCode::Backspace => { self.craps.bet_input.pop(); },
                KeyCode::Enter => {
                    if let Ok(bet) = self.craps.bet_input.parse::<u64>() {
                        if bet > 0 && bet <= self.tokens {
                            let bt = match self.craps.cursor {
                                0 => CrapsBet::Pass,
                                1 => CrapsBet::DontPass,
                                _ => CrapsBet::Field,
                            };
                            self.craps.start_round(bet, bt);
                        }
                    }
                }
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            },
            CrapsPhase::Point => match key.code {
                KeyCode::Char(' ') | KeyCode::Enter => {
                    self.craps.roll_dice();
                    self.craps.phase_timer = 0;
                }
                _ => {}
            },
            CrapsPhase::Result => match key.code {
                KeyCode::Enter => self.craps.new_round(),
                KeyCode::Esc => self.switch_screen(Screen::Hub),
                _ => {}
            },
            _ => {} // during animation
        }
    }

    fn apply_generic_payout(&mut self, payout: i64) {
        if payout > 0 {
            self.tokens += payout as u64;
            self.save.total_earned += payout as u64;
        } else if payout < 0 {
            let loss = (-payout) as u64;
            self.tokens = self.tokens.saturating_sub(loss);
            self.save.total_lost += loss;
        }
        self.save.games_played += 1;
        self.persist();
        self.check_game_over();
    }

    pub fn persist(&mut self) {
        self.save.tokens = self.tokens;
        let _ = crate::save::save(&self.save);
    }

    pub fn check_game_over(&mut self) {
        if self.tokens == 0 {
            self.screen = Screen::GameOver;
        }
    }
}
