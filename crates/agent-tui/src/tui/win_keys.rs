//! Windows console key quirks the TUI undoes before handling a key.
//!
//! **Alt-code characters.** The console delivers a character that is not on
//! the keyboard layout as an Alt-code: Alt down, numpad digits, then Alt up
//! carrying the character. The console host synthesizes exactly that for
//! pasted text and for text typed over ssh or by a terminal that sends plain
//! text, and the user can type one with Alt+numpad. crossterm reports only
//! the last record, as a `Char` key *Release* with no Press.
//! `input::handle_event` drops releases (Windows sends one for every
//! keystroke), so such characters vanished: a paste of `“quotes” → ✨`
//! arrived as `quotes  `. [`AltCodeGate`] tells the two releases apart by
//! what came before: a release that ends a held character key is dropped,
//! and one that matches no held key is the character itself. Only a
//! non-ASCII BMP character is taken that way, so a release that strays in (a
//! key pressed before the window had focus) can never type an ASCII letter;
//! an Alt-code is one UTF-16 unit, so it is never an astral character, and
//! those (emoji) arrive as a Press without a Release, so they are not held
//! either. Windows has no repeat kind: a held key repeats as more Presses of
//! the same character, which count once.
//!
//! **AltGr characters.** Windows reports AltGr as Ctrl+Alt, so `@`, `{`,
//! `\`, `€` on a German layout, `—` on others, arrived as Ctrl+Alt+char,
//! which the editor treats as a shortcut and drops. [`altgr_as_text`] makes
//! such a character plain text. A real Ctrl+Alt+letter shortcut arrives as
//! the ASCII letter, so letters and digits keep their modifiers.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A character key held longer than this without a repeat Press is taken as
/// released (its Release went to another window); a held key repeats every
/// ~30 ms after a delay of at most 1 s.
const STALE: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub(crate) struct AltCodeGate {
    held: u32,
    last_press: Option<(char, Instant)>,
}

/// A character an Alt-code can carry, and that is held between its Press
/// and its Release.
fn paired_non_ascii(c: char) -> bool {
    !c.is_ascii() && u32::from(c) <= 0xFFFF
}

impl AltCodeGate {
    /// A Release to handle as the key it carries (a Press), or `None` to drop
    /// it. Every key event must pass through [`Self::observe`] first.
    pub(crate) fn release_as_press(&mut self, key: &KeyEvent, now: Instant) -> Option<KeyEvent> {
        if key.kind != KeyEventKind::Release {
            return None;
        }
        let KeyCode::Char(c) = key.code else {
            return None;
        };
        if self.last_press.is_some_and(|(_, at)| now.duration_since(at) > STALE) {
            self.held = 0;
            self.last_press = None;
        }
        if self.held > 0 {
            self.held -= 1;
            if self.held == 0 {
                self.last_press = None;
            }
            return None;
        }
        paired_non_ascii(c).then_some(KeyEvent { kind: KeyEventKind::Press, ..*key })
    }

    /// Record a character key's Press (a repeat of the last one counts once).
    pub(crate) fn observe(&mut self, key: &KeyEvent, now: Instant) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let KeyCode::Char(c) = key.code else {
            return;
        };
        if u32::from(c) > 0xFFFF {
            return;
        }
        if !(self.held > 0 && self.last_press.is_some_and(|(last, _)| last == c)) {
            self.held += 1;
        }
        self.last_press = Some((c, now));
    }
}

/// Drop the Ctrl+Alt that AltGr adds to the character it types.
pub(crate) fn altgr_as_text(key: &mut KeyEvent) {
    let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
    if let KeyCode::Char(c) = key.code {
        if key.modifiers.contains(altgr) && !c.is_ascii_alphanumeric() && !c.is_control() {
            key.modifiers.remove(altgr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn ev(c: char, kind: KeyEventKind) -> KeyEvent {
        KeyEvent { code: KeyCode::Char(c), modifiers: KeyModifiers::NONE, kind, state: KeyEventState::empty() }
    }

    /// Feed `(char, press?)` events at 10 ms steps; return what was typed.
    fn typed(events: &[(char, bool)]) -> String {
        let mut gate = AltCodeGate::default();
        let t0 = Instant::now();
        let mut out = String::new();
        for (i, &(c, press)) in events.iter().enumerate() {
            let now = t0 + Duration::from_millis(10 * i as u64);
            let key = ev(c, if press { KeyEventKind::Press } else { KeyEventKind::Release });
            gate.observe(&key, now);
            if press {
                out.push(c);
            } else if let Some(k) = gate.release_as_press(&key, now) {
                assert_eq!(k.kind, KeyEventKind::Press);
                let KeyCode::Char(c) = k.code else { unreachable!() };
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn alt_code_characters_are_typed() {
        // A paste: every non-layout character is a lone Release.
        assert_eq!(typed(&[('a', true), ('a', false), ('“', false), ('→', false), ('b', true), ('b', false)]), "a“→b");
    }

    #[test]
    fn a_keys_own_release_is_dropped_even_when_its_character_changed() {
        // Shift released before the key: Press 'É' ... Release 'é' is still its
        // release, and overlapping keys release in any order.
        assert_eq!(typed(&[('É', true), ('é', false), ('ü', true), ('ö', true), ('ü', false), ('ö', false)]), "Éüö");
    }

    #[test]
    fn a_held_key_repeats_without_swallowing_the_next_alt_code() {
        assert_eq!(typed(&[('ü', true), ('ü', true), ('ü', true), ('ü', false), ('✨', false)]), "üüü✨");
    }

    /// Recorded on Windows 11 over ConPTY: an emoji is a Press with no
    /// Release, and it must not swallow the next Alt-code.
    #[test]
    fn an_emoji_press_without_release_does_not_hold() {
        assert_eq!(typed(&[('🦀', true), ('|', true), ('|', false), ('“', false)]), "🦀|“");
        // An emoji's Release, where a terminal sends one, is not a character.
        assert_eq!(typed(&[('🦀', true), ('🦀', false)]), "🦀");
    }

    #[test]
    fn a_stray_ascii_release_never_types() {
        assert_eq!(typed(&[('a', false), ('Z', false), ('1', false)]), "");
    }

    #[test]
    fn a_held_key_whose_release_went_elsewhere_stops_counting() {
        let mut gate = AltCodeGate::default();
        let t0 = Instant::now();
        gate.observe(&ev('ü', KeyEventKind::Press), t0);
        let later = t0 + STALE + Duration::from_millis(1);
        let k = gate.release_as_press(&ev('✨', KeyEventKind::Release), later);
        assert_eq!(k.map(|k| k.code), Some(KeyCode::Char('✨')));
    }

    #[test]
    fn non_character_releases_are_dropped() {
        let mut gate = AltCodeGate::default();
        let enter = KeyEvent { code: KeyCode::Enter, ..ev('x', KeyEventKind::Release) };
        assert!(gate.release_as_press(&enter, Instant::now()).is_none());
    }

    #[test]
    fn altgr_characters_are_text_and_ctrl_alt_letters_stay_shortcuts() {
        let with = |c, m| KeyEvent { modifiers: m, ..ev(c, KeyEventKind::Press) };
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        for (c, m, want) in [
            ('@', altgr, KeyModifiers::NONE),
            ('€', altgr, KeyModifiers::NONE),
            ('—', altgr | KeyModifiers::SHIFT, KeyModifiers::SHIFT), // recorded on en-IN
            ('q', altgr, altgr),
            ('7', altgr, altgr),
            ('@', KeyModifiers::CONTROL, KeyModifiers::CONTROL),
        ] {
            let mut k = with(c, m);
            altgr_as_text(&mut k);
            assert_eq!(k.modifiers, want, "{c:?} with {m:?}");
        }
    }
}
