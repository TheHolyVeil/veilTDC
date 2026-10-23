//! Magic SysRq while Veil holds the keyboards (K2).
//!
//! Veil grabs every input device exclusively (`EVIOCGRAB`), so the kernel's
//! SysRq handler never sees the keys and Alt+SysRq+<letter> (REISUB and
//! friends) silently stopped working while Veil ran. The kernel's handler
//! *does* listen to every input device, including virtual ones, so we bridge
//! the chord: when Alt+SysRq is pressed we replay the chord, and the letters
//! typed while SysRq is held, on a private uinput keyboard.
//!
//! This needs write access to `/dev/uinput` (group `input` on most distros)
//! and `kernel.sysrq` allowing the action. If either is missing the bridge
//! is simply off and everything behaves as before.
//!
//! The decision logic is pure (`Chord`) and unit-tested; the uinput part is a
//! thin shell around it.

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};

pub const KEY_SYSRQ: u16 = 99;
const KEY_LEFTALT: u16 = 56;

fn is_modifier(code: u16) -> bool {
    matches!(code, 29 | 42 | 54 | 56 | 97 | 100 | 125 | 126)
}

/// What the chord logic wants done with one key event.
#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    /// Events to replay on the virtual keyboard, in order: `(code, value)`.
    pub emit: Vec<(u16, i32)>,
    /// True if Veil must not process the event itself.
    pub consume: bool,
}

impl Decision {
    fn pass() -> Self {
        Self {
            emit: Vec::new(),
            consume: false,
        }
    }
}

/// The Alt+SysRq chord state machine.
#[derive(Default)]
pub struct Chord {
    active: bool,
}

impl Chord {
    pub fn reset(&mut self) {
        self.active = false;
    }

    /// Feed one physical key event (`value`: 0 release, 1 press, 2 repeat).
    pub fn feed(&mut self, code: u16, value: i32, alt_held: bool) -> Decision {
        if !self.active {
            if code == KEY_SYSRQ && value == 1 && alt_held {
                self.active = true;
                return Decision {
                    emit: vec![(KEY_LEFTALT, 1), (KEY_SYSRQ, 1)],
                    consume: true,
                };
            }
            return Decision::pass();
        }

        if code == KEY_SYSRQ && value == 0 {
            self.active = false;
            return Decision {
                emit: vec![(KEY_SYSRQ, 0), (KEY_LEFTALT, 0)],
                consume: true,
            };
        }
        if is_modifier(code) {
            // Mirror it, but let Veil see it too so its modifier state (and
            // the Ctrl+Alt+Fn / kill chords) stay correct.
            return Decision {
                emit: vec![(code, value)],
                consume: false,
            };
        }
        Decision {
            emit: vec![(code, value)],
            consume: true,
        }
    }
}

/// The uinput side. `dev` is `None` when `/dev/uinput` is not usable.
pub struct SysRqBridge {
    dev: Option<VirtualDevice>,
    chord: Chord,
}

impl SysRqBridge {
    pub fn new() -> Self {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=248u16 {
            keys.insert(KeyCode::new(code));
        }
        let dev = VirtualDevice::builder()
            .and_then(|b| b.name("veil-sysrq").with_keys(&keys))
            .and_then(|b| b.build());
        let dev = match dev {
            Ok(d) => {
                eprintln!("[veil-host] evdev: SysRq bridge ready (Alt+SysRq+<key> works under Veil)");
                Some(d)
            }
            Err(e) => {
                eprintln!(
                    "[veil-host] evdev: SysRq bridge off ({e}); Alt+SysRq won't reach the kernel while Veil runs. \
                     It needs write access to /dev/uinput (e.g. a udev rule giving group `input` mode 0660)."
                );
                None
            }
        };
        Self {
            dev,
            chord: Chord::default(),
        }
    }

    pub fn reset(&mut self) {
        self.chord.reset();
    }

    /// Returns true if the event was consumed and Veil should ignore it.
    pub fn handle(&mut self, code: u16, value: i32, alt_held: bool) -> bool {
        let Some(dev) = self.dev.as_mut() else {
            return false;
        };
        let d = self.chord.feed(code, value, alt_held);
        if !d.emit.is_empty() {
            let events: Vec<InputEvent> = d
                .emit
                .iter()
                .map(|&(c, v)| InputEvent::new(EventType::KEY.0, c, v))
                .collect();
            if let Err(e) = dev.emit(&events) {
                eprintln!("[veil-host] evdev: SysRq bridge write failed: {e}");
            }
        }
        d.consume
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_B: u16 = 48;
    const KEY_LEFTCTRL: u16 = 29;

    #[test]
    fn plain_sysrq_without_alt_is_left_alone() {
        let mut c = Chord::default();
        assert_eq!(c.feed(KEY_SYSRQ, 1, false), Decision::pass());
        assert_eq!(c.feed(KEY_B, 1, false), Decision::pass());
    }

    #[test]
    fn alt_sysrq_then_letter_is_replayed_and_consumed() {
        let mut c = Chord::default();
        let d = c.feed(KEY_SYSRQ, 1, true);
        assert_eq!(d.emit, vec![(KEY_LEFTALT, 1), (KEY_SYSRQ, 1)]);
        assert!(d.consume);

        let d = c.feed(KEY_B, 1, true);
        assert_eq!(d.emit, vec![(KEY_B, 1)]);
        assert!(d.consume);
        let d = c.feed(KEY_B, 0, true);
        assert_eq!(d.emit, vec![(KEY_B, 0)]);
        assert!(d.consume);

        let d = c.feed(KEY_SYSRQ, 0, true);
        assert_eq!(d.emit, vec![(KEY_SYSRQ, 0), (KEY_LEFTALT, 0)]);
        assert!(d.consume);
    }

    #[test]
    fn modifiers_are_mirrored_but_still_reach_veil() {
        let mut c = Chord::default();
        c.feed(KEY_SYSRQ, 1, true);
        let d = c.feed(KEY_LEFTCTRL, 1, true);
        assert_eq!(d.emit, vec![(KEY_LEFTCTRL, 1)]);
        assert!(!d.consume, "Veil must still track the modifier");
        let d = c.feed(KEY_LEFTALT, 0, true);
        assert_eq!(d.emit, vec![(KEY_LEFTALT, 0)]);
        assert!(!d.consume);
    }

    #[test]
    fn chord_ends_cleanly_and_normal_keys_flow_again() {
        let mut c = Chord::default();
        c.feed(KEY_SYSRQ, 1, true);
        c.feed(KEY_SYSRQ, 0, true);
        assert_eq!(c.feed(KEY_B, 1, false), Decision::pass());
    }

    #[test]
    fn reset_abandons_a_chord() {
        let mut c = Chord::default();
        c.feed(KEY_SYSRQ, 1, true);
        c.reset();
        assert_eq!(c.feed(KEY_B, 1, false), Decision::pass());
    }
}
