//! Disable-while-typing for the touchpad.
//!
//! A palm resting on the pad while you type moves the pointer or taps by
//! accident. The usual fix (libinput's) is to ignore the pad for a short
//! while after every keypress. This does that, with two refinements so it
//! stays out of the way when you are not typing text:
//!
//! * **Shortcuts do not count.** Modifiers alone never block, and a key
//!   pressed while Ctrl, Alt or Super is held is a shortcut, not typing. So
//!   Ctrl+click and Super+drag keep working.
//! * **Gaming does not count.** Holding a key down for a while (movement) or
//!   a run of presses made only of the usual game keys (WASD, arrows, the
//!   number row, space, tab, Q/E/R/F) marks the keyboard as a game
//!   controller, and the pad stays live until a key outside that set is
//!   pressed. Typing ordinary text switches the block straight back on.
//!
//! Pure and unit-tested: the caller passes the time.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long after the last typed key the pad stays disabled.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(500);
/// A non-text key held this long is held for movement, not typed.
const GAME_HOLD: Duration = Duration::from_millis(300);
/// This many presses in a row, all game keys and all this recent, is a game.
const GAME_STREAK: usize = 6;
const GAME_WINDOW: Duration = Duration::from_millis(2500);
/// Once gaming has been seen the pad stays live this long without more
/// evidence (tap-strafing has gaps), or until an ordinary typed key.
const GAME_GRACE: Duration = Duration::from_secs(3);

// Evdev key codes.
const KEY_BACKSPACE: u16 = 14;
const KEY_ENTER: u16 = 28;

/// Keys that mean "a game", when they are the only thing being pressed.
const GAME_KEYS: &[u16] = &[
    15, // Tab
    2, 3, 4, 5, 6, // 1..5
    16, 18, 19, // Q E R
    17, 30, 31, 32, // W A S D
    33,  // F
    57,  // Space
    103, 105, 106, 108, // arrows
];

/// Ctrl, Alt and Super (either side) make a chord a shortcut.
fn shortcut_bit(code: u16) -> Option<u8> {
    Some(match code {
        29 => 1,   // left ctrl
        97 => 2,   // right ctrl
        56 => 4,   // left alt
        100 => 8,  // right alt
        125 => 16, // left meta
        126 => 32, // right meta
        _ => return None,
    })
}

/// Keys whose presses mean the hands are on the keyboard writing: letters,
/// digits, punctuation, space, enter, backspace, tab, the numpad and arrows.
/// Escape, function keys, media keys, lock keys and shift are not.
fn is_typing_key(code: u16) -> bool {
    matches!(code,
        2..=14        // 1..0 - = backspace
        | 15..=28     // tab, QWERTYUIOP [ ] enter
        | 30..=41     // ASDFGHJKL ; ' `
        | 43..=53     // \ ZXCVBNM , . /
        | 57          // space
        | 71..=83     // numpad
        | 96          // numpad enter
        | 103 | 105 | 106 | 108 // arrows
    )
}

pub struct Dwt {
    /// `None` = disabled.
    timeout: Option<Duration>,
    last_typed: Option<Instant>,
    /// Typing keys currently down, and since when.
    held: Vec<(u16, Instant)>,
    /// The last `GAME_STREAK` typing-key presses.
    recent: VecDeque<(u16, Instant)>,
    /// Held Ctrl/Alt/Super bits.
    mods: u8,
    /// The pad is left alone until this time (gaming seen).
    game_until: Option<Instant>,
}

impl Dwt {
    pub fn new(timeout: Option<Duration>) -> Self {
        Self {
            timeout,
            last_typed: None,
            held: Vec::new(),
            recent: VecDeque::with_capacity(GAME_STREAK + 1),
            mods: 0,
            game_until: None,
        }
    }

    /// Default on. `VEIL_DWT` overrides: `off` / `0` / `false` / `no`
    /// disables it, a number is the timeout in milliseconds.
    pub fn from_env() -> Self {
        Self::new(parse_setting(std::env::var("VEIL_DWT").ok().as_deref()))
    }

    /// Forget everything (session went inactive).
    pub fn reset(&mut self) {
        *self = Self::new(self.timeout);
    }

    /// A key press or release on a keyboard. Mouse and touchpad buttons
    /// (codes 0x100 and up) and autorepeat are not for here.
    pub fn key(&mut self, code: u16, pressed: bool, now: Instant) {
        if let Some(bit) = shortcut_bit(code) {
            if pressed {
                self.mods |= bit;
            } else {
                self.mods &= !bit;
            }
            return;
        }
        if !is_typing_key(code) {
            return;
        }
        if !pressed {
            // Releasing a key that was held for movement: still gaming.
            if let Some(&(_, t)) = self.held.iter().find(|(c, _)| *c == code) {
                if self.is_game_hold(code, t, now) {
                    self.game_until = Some(now + GAME_GRACE);
                }
            }
            self.held.retain(|(c, _)| *c != code);
            return;
        }
        if self.mods != 0 {
            return; // a shortcut chord, not text
        }
        self.held.retain(|(c, _)| *c != code);
        self.held.push((code, now));
        self.last_typed = Some(now);
        if !GAME_KEYS.contains(&code) {
            // Real text: whatever game was going on has stopped.
            self.game_until = None;
        }
        self.recent.push_back((code, now));
        while self.recent.len() > GAME_STREAK {
            self.recent.pop_front();
        }
        if self.is_game_streak(now) {
            self.game_until = Some(now + GAME_GRACE);
        }
    }

    fn is_game_hold(&self, code: u16, since: Instant, now: Instant) -> bool {
        // Holding backspace or enter is deleting / entering, not moving.
        code != KEY_BACKSPACE
            && code != KEY_ENTER
            && now.saturating_duration_since(since) >= GAME_HOLD
    }

    fn is_game_streak(&self, now: Instant) -> bool {
        self.recent.len() == GAME_STREAK
            && self.recent.iter().all(|(c, t)| {
                GAME_KEYS.contains(c) && now.saturating_duration_since(*t) <= GAME_WINDOW
            })
    }

    /// Should the touchpad be ignored right now?
    pub fn blocks(&self, now: Instant) -> bool {
        let Some(timeout) = self.timeout else {
            return false;
        };
        if self.game_until.is_some_and(|t| now < t) {
            return false;
        }
        if self.held.iter().any(|&(c, t)| self.is_game_hold(c, t, now)) {
            return false;
        }
        self.last_typed
            .is_some_and(|t| now.saturating_duration_since(t) < timeout)
    }
}

/// `VEIL_DWT` → timeout. Unset or unparsable = the default.
fn parse_setting(v: Option<&str>) -> Option<Duration> {
    match v.map(str::trim) {
        None | Some("") => Some(DEFAULT_TIMEOUT),
        Some(s) if matches!(s.to_ascii_lowercase().as_str(), "off" | "false" | "no" | "0") => None,
        Some(s) => match s.parse::<u64>() {
            Ok(ms) => Some(Duration::from_millis(ms.clamp(50, 5000))),
            Err(_) => Some(DEFAULT_TIMEOUT),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_H: u16 = 35;
    const KEY_I: u16 = 23;
    const KEY_W: u16 = 17;
    const KEY_A: u16 = 30;
    const KEY_S: u16 = 31;
    const KEY_D: u16 = 32;
    const KEY_X: u16 = 45;
    const KEY_LSHIFT: u16 = 42;
    const KEY_LCTRL: u16 = 29;
    const KEY_LMETA: u16 = 125;
    const KEY_C: u16 = 46;
    const KEY_F5: u16 = 63;
    const KEY_VOLUMEUP: u16 = 115;
    const BTN_LEFT: u16 = 0x110;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn tap(d: &mut Dwt, code: u16, t: Instant) {
        d.key(code, true, t);
        d.key(code, false, t + ms(40));
    }

    #[test]
    fn typing_blocks_then_the_block_expires() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        assert!(!d.blocks(t), "idle keyboard: pad is live");
        tap(&mut d, KEY_H, t);
        assert!(d.blocks(t + ms(100)));
        assert!(d.blocks(t + ms(450)));
        assert!(!d.blocks(t + ms(500)));
        assert!(!d.blocks(t + ms(2000)));
    }

    #[test]
    fn each_key_extends_the_block() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        tap(&mut d, KEY_H, t);
        tap(&mut d, KEY_I, t + ms(400));
        assert!(d.blocks(t + ms(800)), "second key renewed it");
        assert!(!d.blocks(t + ms(950)));
    }

    #[test]
    fn modifiers_alone_never_block_but_shift_plus_a_letter_does() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        d.key(KEY_LSHIFT, true, t);
        d.key(KEY_LCTRL, true, t);
        d.key(KEY_LMETA, true, t);
        assert!(!d.blocks(t + ms(10)));
        d.key(KEY_LCTRL, false, t + ms(20));
        d.key(KEY_LMETA, false, t + ms(20));
        d.key(KEY_H, true, t + ms(30)); // Shift+H: a capital letter
        assert!(d.blocks(t + ms(60)));
    }

    #[test]
    fn shortcut_chords_do_not_block() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        d.key(KEY_LCTRL, true, t);
        tap(&mut d, KEY_C, t + ms(10)); // Ctrl+C
        assert!(!d.blocks(t + ms(60)));
        d.key(KEY_LCTRL, false, t + ms(100));
        d.key(KEY_LMETA, true, t + ms(200));
        tap(&mut d, KEY_H, t + ms(210)); // Super+H
        assert!(!d.blocks(t + ms(260)));
        d.key(KEY_LMETA, false, t + ms(300));
        // Typing after the chord blocks as usual.
        tap(&mut d, KEY_H, t + ms(400));
        assert!(d.blocks(t + ms(450)));
    }

    #[test]
    fn non_text_keys_and_buttons_do_not_block() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        tap(&mut d, KEY_F5, t);
        tap(&mut d, KEY_VOLUMEUP, t);
        tap(&mut d, 1, t); // Escape
        tap(&mut d, BTN_LEFT, t); // the pad's own click
        assert!(!d.blocks(t + ms(50)));
    }

    #[test]
    fn a_held_movement_key_means_gaming_and_leaves_the_pad_live() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        d.key(KEY_W, true, t);
        assert!(d.blocks(t + ms(100)), "just pressed: could be typing");
        assert!(!d.blocks(t + ms(350)), "held for movement");
        assert!(!d.blocks(t + ms(2000)));
        d.key(KEY_W, false, t + ms(2100));
        // A gap in movement does not switch the block back on.
        assert!(!d.blocks(t + ms(2200)));
        assert!(!d.blocks(t + ms(4000)));
    }

    #[test]
    fn tapping_only_game_keys_is_gaming() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        let seq = [KEY_W, KEY_A, KEY_S, KEY_D, KEY_W, KEY_D];
        for (i, k) in seq.iter().enumerate() {
            tap(&mut d, *k, t + ms(150 * i as u64));
        }
        // After six game-key presses the pad is live even right after one.
        assert!(!d.blocks(t + ms(150 * 5 + 60)));
        // Short words made of game keys do not trip it (needs a streak).
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        tap(&mut d, KEY_W, t);
        tap(&mut d, KEY_A, t + ms(100));
        tap(&mut d, KEY_S, t + ms(200));
        assert!(d.blocks(t + ms(260)), "'was' is typing");
    }

    #[test]
    fn typing_a_real_letter_ends_the_gaming_state_at_once() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        d.key(KEY_W, true, t);
        assert!(!d.blocks(t + ms(400)));
        d.key(KEY_W, false, t + ms(500));
        assert!(!d.blocks(t + ms(600)));
        // Alt-tab to a chat window and type.
        tap(&mut d, KEY_X, t + ms(700));
        assert!(d.blocks(t + ms(760)));
    }

    #[test]
    fn holding_backspace_is_deleting_not_gaming() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        d.key(KEY_BACKSPACE, true, t);
        d.key(KEY_BACKSPACE, true, t + ms(400)); // (autorepeat does not reach us)
        // Still held 400 ms later; the key was pressed at t, but a held
        // backspace must not unblock the pad.
        assert!(!d.is_game_hold(KEY_BACKSPACE, t, t + ms(400)));
    }

    #[test]
    fn disabled_never_blocks() {
        let mut d = Dwt::new(None);
        let t = Instant::now();
        tap(&mut d, KEY_H, t);
        assert!(!d.blocks(t + ms(10)));
    }

    #[test]
    fn reset_forgets_typing_and_modifiers() {
        let mut d = Dwt::new(Some(DEFAULT_TIMEOUT));
        let t = Instant::now();
        d.key(KEY_LCTRL, true, t);
        d.reset();
        tap(&mut d, KEY_H, t + ms(10)); // ctrl is forgotten: this is typing
        assert!(d.blocks(t + ms(60)));
        d.reset();
        assert!(!d.blocks(t + ms(60)));
    }

    #[test]
    fn setting_parsing() {
        assert_eq!(parse_setting(None), Some(DEFAULT_TIMEOUT));
        assert_eq!(parse_setting(Some("")), Some(DEFAULT_TIMEOUT));
        assert_eq!(parse_setting(Some("off")), None);
        assert_eq!(parse_setting(Some("OFF")), None);
        assert_eq!(parse_setting(Some("0")), None);
        assert_eq!(parse_setting(Some("false")), None);
        assert_eq!(parse_setting(Some("800")), Some(ms(800)));
        assert_eq!(parse_setting(Some("5")), Some(ms(50)), "clamped up");
        assert_eq!(parse_setting(Some("99999")), Some(ms(5000)), "clamped down");
        assert_eq!(parse_setting(Some("banana")), Some(DEFAULT_TIMEOUT));
    }
}
