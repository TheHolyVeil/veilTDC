//! Touchpad support for the bare-TTY evdev backend.
//!
//! A mouse reports relative motion (REL_X/REL_Y). A touchpad does not: it
//! reports where each finger *is* (ABS_X/ABS_Y, in device units) plus
//! BTN_TOUCH / BTN_TOOL_* flags saying how many fingers are down. Without this
//! module the pointer never moved on a laptop TTY, and the BTN_TOUCH/BTN_TOOL
//! flags were forwarded to clients as keystrokes.
//!
//! This is a small state machine, fed raw events and flushed on SYN_REPORT:
//! - one finger moves the pointer (with a mild speed-dependent gain),
//! - two fingers scroll vertically,
//! - a quick, short tap is a left click (the physical click is plain BTN_LEFT
//!   and is handled by the caller).
//!
//! It is pure (time is passed in), so it is unit-tested without a device.

use std::time::{Duration, Instant};

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_TOOL_FINGER: u16 = 0x145;
pub const BTN_TOOL_DOUBLETAP: u16 = 0x14d;
pub const BTN_TOOL_TRIPLETAP: u16 = 0x14e;
pub const BTN_TOOL_QUADTAP: u16 = 0x14f;

/// Pointer pixels per millimetre of finger travel, before gain.
const PX_PER_MM: f32 = 18.0;
/// Gain grows with speed (mm per report) up to this multiple.
const GAIN_PER_MM: f32 = 1.2;
const GAIN_MAX: f32 = 2.5;
/// Two-finger scroll: one wheel notch (v120 = 120) per this many mm.
const MM_PER_NOTCH: f32 = 4.0;
/// Emit scroll once at least this much of a notch has built up (v120 units).
const SCROLL_MIN_V120: f32 = 15.0;
/// Content follows the fingers (finger down => view scrolls up).
const NATURAL_SCROLL: bool = true;
/// A tap is a touch shorter than this that barely moved.
const TAP_MAX_TIME: Duration = Duration::from_millis(200);
const TAP_MAX_TRAVEL_MM: f32 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    /// Pointer motion in compositor pixels.
    Motion(i32, i32),
    /// Scroll, in the compositor's v120 units (+120 = one notch down).
    Scroll(i32),
    /// A tap: send a left press + release.
    Tap,
}

pub struct Touchpad {
    /// Device units per millimetre.
    res_x: f32,
    res_y: f32,
    x: i32,
    y: i32,
    have_x: bool,
    have_y: bool,
    touching: bool,
    tool_finger: bool,
    tool_double: bool,
    tool_many: bool,
    last: Option<(i32, i32)>,
    touch_started: Option<Instant>,
    travelled_mm: f32,
    peak_fingers: u8,
    pending_tap: bool,
    scroll_acc_mm: f32,
    rem_x: f32,
    rem_y: f32,
}

impl Touchpad {
    /// `res_x`/`res_y` are device units per mm (from the axis resolution).
    pub fn new(res_x: f32, res_y: f32) -> Self {
        Self {
            res_x: res_x.max(0.1),
            res_y: res_y.max(0.1),
            x: 0,
            y: 0,
            have_x: false,
            have_y: false,
            touching: false,
            tool_finger: false,
            tool_double: false,
            tool_many: false,
            last: None,
            touch_started: None,
            travelled_mm: 0.0,
            peak_fingers: 0,
            pending_tap: false,
            scroll_acc_mm: 0.0,
            rem_x: 0.0,
            rem_y: 0.0,
        }
    }

    /// Forget any touch in progress (used when the session goes inactive).
    pub fn reset(&mut self) {
        let (rx, ry) = (self.res_x, self.res_y);
        *self = Self::new(rx, ry);
    }

    fn fingers(&self) -> u8 {
        if self.tool_many {
            3
        } else if self.tool_double {
            2
        } else if self.tool_finger {
            1
        } else {
            0
        }
    }

    /// A BTN_TOUCH or BTN_TOOL_* event. Returns true if it was one of ours
    /// (so the caller must not forward it as a keystroke).
    pub fn key(&mut self, code: u16, pressed: bool, now: Instant) -> bool {
        let before = self.fingers();
        match code {
            BTN_TOUCH => {
                if pressed {
                    self.touching = true;
                    self.touch_started = Some(now);
                    self.travelled_mm = 0.0;
                    self.peak_fingers = 0;
                    self.scroll_acc_mm = 0.0;
                    self.last = None;
                } else {
                    let quick = self
                        .touch_started
                        .is_some_and(|t| now.duration_since(t) < TAP_MAX_TIME);
                    if self.touching
                        && quick
                        && self.peak_fingers == 1
                        && self.travelled_mm < TAP_MAX_TRAVEL_MM
                    {
                        self.pending_tap = true;
                    }
                    self.touching = false;
                    self.last = None;
                }
            }
            BTN_TOOL_FINGER => self.tool_finger = pressed,
            BTN_TOOL_DOUBLETAP => self.tool_double = pressed,
            BTN_TOOL_TRIPLETAP | BTN_TOOL_QUADTAP => self.tool_many = pressed,
            _ => return false,
        }
        let after = self.fingers();
        if after != before {
            // The tracked point jumps when the finger count changes; rebase.
            self.last = None;
            self.rem_x = 0.0;
            self.rem_y = 0.0;
            self.peak_fingers = self.peak_fingers.max(after);
        }
        true
    }

    /// An ABS_X / ABS_Y event. Returns true if it was one of ours.
    pub fn abs(&mut self, axis: u16, value: i32) -> bool {
        match axis {
            ABS_X => {
                self.x = value;
                self.have_x = true;
                true
            }
            ABS_Y => {
                self.y = value;
                self.have_y = true;
                true
            }
            _ => false,
        }
    }

    /// SYN_REPORT: turn what accumulated this frame into outputs.
    pub fn sync(&mut self, out: &mut Vec<Out>) {
        if self.pending_tap {
            self.pending_tap = false;
            out.push(Out::Tap);
        }
        let fingers = self.fingers();
        if !(self.touching && fingers >= 1 && self.have_x && self.have_y) {
            return;
        }
        let Some((lx, ly)) = self.last.replace((self.x, self.y)) else {
            return; // first report of a touch: just remember where it is
        };
        let dx_mm = (self.x - lx) as f32 / self.res_x;
        let dy_mm = (self.y - ly) as f32 / self.res_y;
        self.travelled_mm += dx_mm.hypot(dy_mm);

        match fingers {
            1 => {
                let gain = (1.0 + dx_mm.hypot(dy_mm) * GAIN_PER_MM).min(GAIN_MAX);
                let fx = self.rem_x + dx_mm * PX_PER_MM * gain;
                let fy = self.rem_y + dy_mm * PX_PER_MM * gain;
                let (ix, iy) = (fx.trunc(), fy.trunc());
                self.rem_x = fx - ix;
                self.rem_y = fy - iy;
                if ix != 0.0 || iy != 0.0 {
                    out.push(Out::Motion(ix as i32, iy as i32));
                }
            }
            2 => {
                self.scroll_acc_mm += dy_mm;
                let v120 = self.scroll_acc_mm * (120.0 / MM_PER_NOTCH);
                if v120.abs() >= SCROLL_MIN_V120 {
                    self.scroll_acc_mm -= v120.trunc() / (120.0 / MM_PER_NOTCH);
                    let signed = if NATURAL_SCROLL { -v120 } else { v120 };
                    out.push(Out::Scroll(signed.trunc() as i32));
                }
            }
            _ => {} // 3+ fingers: reserved (no gestures yet)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 units per mm both ways keeps the numbers easy to read.
    fn pad() -> Touchpad {
        Touchpad::new(10.0, 10.0)
    }

    fn frame(tp: &mut Touchpad, x: i32, y: i32) -> Vec<Out> {
        tp.abs(ABS_X, x);
        tp.abs(ABS_Y, y);
        let mut out = Vec::new();
        tp.sync(&mut out);
        out
    }

    fn touch(tp: &mut Touchpad, now: Instant, fingers: u16) {
        tp.key(BTN_TOUCH, true, now);
        tp.key(
            match fingers {
                1 => BTN_TOOL_FINGER,
                2 => BTN_TOOL_DOUBLETAP,
                _ => BTN_TOOL_TRIPLETAP,
            },
            true,
            now,
        );
    }

    #[test]
    fn first_report_of_a_touch_never_jumps_the_pointer() {
        let mut tp = pad();
        let t = Instant::now();
        touch(&mut tp, t, 1);
        assert!(frame(&mut tp, 500, 500).is_empty());
    }

    #[test]
    fn one_finger_moves_the_pointer_both_ways() {
        let mut tp = pad();
        let t = Instant::now();
        touch(&mut tp, t, 1);
        frame(&mut tp, 500, 500);
        // 10 units = 1 mm right, 1 mm down => about PX_PER_MM px (+ gain).
        let out = frame(&mut tp, 510, 510);
        match out.as_slice() {
            [Out::Motion(dx, dy)] => {
                assert!(*dx > 15 && *dx < 60, "dx={dx}");
                assert_eq!(dx, dy);
            }
            other => panic!("unexpected {other:?}"),
        }
        let back = frame(&mut tp, 500, 500);
        assert!(matches!(back.as_slice(), [Out::Motion(dx, _)] if *dx < 0));
    }

    #[test]
    fn small_moves_keep_their_fraction() {
        let mut tp = pad();
        let t = Instant::now();
        touch(&mut tp, t, 1);
        frame(&mut tp, 500, 500);
        // 1 unit = 0.1 mm = ~1.8 px: never lose the remainder.
        let mut total = 0;
        for i in 1..=20 {
            for o in frame(&mut tp, 500 + i, 500) {
                if let Out::Motion(dx, _) = o {
                    total += dx;
                }
            }
        }
        assert!(total >= 30, "total={total}");
    }

    #[test]
    fn lifting_and_replacing_a_finger_does_not_jump() {
        let mut tp = pad();
        let t = Instant::now();
        touch(&mut tp, t, 1);
        frame(&mut tp, 100, 100);
        frame(&mut tp, 110, 100);
        tp.key(BTN_TOOL_FINGER, false, t);
        tp.key(BTN_TOUCH, false, t + Duration::from_millis(500));
        let mut out = Vec::new();
        tp.sync(&mut out);
        assert!(out.is_empty());
        // New touch somewhere else entirely: still no jump.
        touch(&mut tp, t + Duration::from_secs(1), 1);
        assert!(frame(&mut tp, 900, 700).is_empty());
    }

    #[test]
    fn two_fingers_scroll_and_do_not_move() {
        let mut tp = pad();
        let t = Instant::now();
        touch(&mut tp, t, 2);
        frame(&mut tp, 500, 500);
        // 4 mm down = one full notch; natural scrolling => negative (up).
        let out = frame(&mut tp, 500, 540);
        assert_eq!(out, vec![Out::Scroll(-120)]);
        assert!(frame(&mut tp, 500, 540).is_empty());
        // And back up: positive.
        let out = frame(&mut tp, 500, 500);
        assert_eq!(out, vec![Out::Scroll(120)]);
    }

    #[test]
    fn changing_finger_count_rebases() {
        let mut tp = pad();
        let t = Instant::now();
        touch(&mut tp, t, 1);
        frame(&mut tp, 100, 100);
        // Second finger lands; the tracked point jumps by 300 units.
        tp.key(BTN_TOOL_FINGER, false, t);
        tp.key(BTN_TOOL_DOUBLETAP, true, t);
        assert!(frame(&mut tp, 400, 400).is_empty(), "no scroll/move on the jump");
    }

    #[test]
    fn quick_still_touch_is_a_tap() {
        let mut tp = pad();
        let t = Instant::now();
        touch(&mut tp, t, 1);
        frame(&mut tp, 500, 500);
        frame(&mut tp, 501, 500);
        tp.key(BTN_TOOL_FINGER, false, t);
        tp.key(BTN_TOUCH, false, t + Duration::from_millis(90));
        let mut out = Vec::new();
        tp.sync(&mut out);
        assert_eq!(out, vec![Out::Tap]);
    }

    #[test]
    fn slow_or_moving_touches_are_not_taps() {
        // Too slow.
        let mut tp = pad();
        let t = Instant::now();
        touch(&mut tp, t, 1);
        frame(&mut tp, 500, 500);
        tp.key(BTN_TOUCH, false, t + Duration::from_millis(400));
        let mut out = Vec::new();
        tp.sync(&mut out);
        assert!(!out.contains(&Out::Tap));

        // Moved too far (5 mm).
        let mut tp = pad();
        touch(&mut tp, t, 1);
        frame(&mut tp, 500, 500);
        frame(&mut tp, 550, 500);
        tp.key(BTN_TOUCH, false, t + Duration::from_millis(100));
        let mut out = Vec::new();
        tp.sync(&mut out);
        assert!(!out.contains(&Out::Tap));

        // Two fingers: not a left tap.
        let mut tp = pad();
        touch(&mut tp, t, 2);
        frame(&mut tp, 500, 500);
        tp.key(BTN_TOUCH, false, t + Duration::from_millis(80));
        let mut out = Vec::new();
        tp.sync(&mut out);
        assert!(!out.contains(&Out::Tap));
    }

    #[test]
    fn unrelated_codes_are_not_swallowed() {
        let mut tp = pad();
        assert!(!tp.key(0x110, true, Instant::now())); // BTN_LEFT: caller's job
        assert!(!tp.abs(0x35, 1)); // ABS_MT_POSITION_X: ignored, not ours
    }
}
