//! Touchscreen support for the bare-TTY evdev backend. Deliberately simple.
//!
//! A touchscreen reports absolute positions (ABS_X/ABS_Y, device units) and
//! BTN_TOUCH. We map that straight onto the compositor's pointer:
//! touching down moves the pointer under your finger and presses the left
//! button, sliding drags, lifting releases. A tap is therefore a click, and a
//! slide is a drag. No gestures, no multi-touch (the first finger wins; the
//! kernel's single-touch emulation does the work).
//!
//! Pure and unit-tested; the evdev loop feeds it events and flushes on
//! SYN_REPORT.

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const BTN_TOUCH: u16 = 0x14a;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Out {
    /// Pointer position as a fraction of the output, each in `0.0..=1.0`.
    MoveTo(f32, f32),
    /// Left button down.
    Press,
    /// Left button up.
    Release,
}

pub struct Touchscreen {
    min_x: i32,
    max_x: i32,
    min_y: i32,
    max_y: i32,
    x: i32,
    y: i32,
    have_x: bool,
    have_y: bool,
    /// Press sent and not yet released.
    down: bool,
    pending_down: bool,
    pending_up: bool,
    moved: bool,
}

impl Touchscreen {
    /// `(min, max)` of the X and Y axes, in device units.
    pub fn new(x_range: (i32, i32), y_range: (i32, i32)) -> Self {
        Self {
            min_x: x_range.0,
            max_x: x_range.1,
            min_y: y_range.0,
            max_y: y_range.1,
            x: 0,
            y: 0,
            have_x: false,
            have_y: false,
            down: false,
            pending_down: false,
            pending_up: false,
            moved: false,
        }
    }

    /// Drop any touch in progress (session went inactive). The caller is
    /// responsible for having released the button if one was down.
    pub fn reset(&mut self) {
        *self = Self::new((self.min_x, self.max_x), (self.min_y, self.max_y));
    }

    /// Whether a press is currently held (so the caller can release it).
    pub fn is_down(&self) -> bool {
        self.down
    }

    /// A key-type event. Returns true if it belongs to the touchscreen and
    /// must not be forwarded as a keystroke (BTN_TOUCH and the BTN_TOOL_*
    /// range, 0x140..=0x15f).
    pub fn key(&mut self, code: u16, pressed: bool) -> bool {
        if code == BTN_TOUCH {
            if pressed {
                self.pending_down = true;
            } else {
                self.pending_up = true;
            }
            return true;
        }
        (0x140..=0x15f).contains(&code)
    }

    /// An absolute-axis event. Other axes (multitouch slots, pressure) are
    /// ignored.
    pub fn abs(&mut self, axis: u16, value: i32) {
        match axis {
            ABS_X => {
                self.x = value;
                self.have_x = true;
                self.moved = true;
            }
            ABS_Y => {
                self.y = value;
                self.have_y = true;
                self.moved = true;
            }
            _ => {}
        }
    }

    fn fraction(&self) -> (f32, f32) {
        let fx = (self.x - self.min_x) as f32 / (self.max_x - self.min_x).max(1) as f32;
        let fy = (self.y - self.min_y) as f32 / (self.max_y - self.min_y).max(1) as f32;
        (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0))
    }

    /// SYN_REPORT: emit what this frame amounts to.
    pub fn sync(&mut self, out: &mut Vec<Out>) {
        let have_pos = self.have_x && self.have_y;
        if self.pending_down && have_pos {
            let (fx, fy) = self.fraction();
            out.push(Out::MoveTo(fx, fy));
            out.push(Out::Press);
            self.down = true;
            self.pending_down = false;
        } else if self.moved && self.down && have_pos {
            let (fx, fy) = self.fraction();
            out.push(Out::MoveTo(fx, fy));
        }
        self.moved = false;

        if self.pending_up {
            if self.down {
                out.push(Out::Release);
                self.down = false;
            }
            self.pending_up = false;
            // A touch that ended before we ever saw a position is dropped.
            self.pending_down = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Touchscreen {
        Touchscreen::new((0, 1000), (0, 500))
    }

    fn frame(ts: &mut Touchscreen) -> Vec<Out> {
        let mut out = Vec::new();
        ts.sync(&mut out);
        out
    }

    #[test]
    fn tap_is_move_press_release() {
        let mut ts = screen();
        ts.key(BTN_TOUCH, true);
        ts.abs(ABS_X, 500);
        ts.abs(ABS_Y, 250);
        assert_eq!(frame(&mut ts), vec![Out::MoveTo(0.5, 0.5), Out::Press]);
        assert!(ts.is_down());
        ts.key(BTN_TOUCH, false);
        assert_eq!(frame(&mut ts), vec![Out::Release]);
        assert!(!ts.is_down());
    }

    #[test]
    fn very_quick_tap_in_one_frame_still_clicks() {
        let mut ts = screen();
        ts.abs(ABS_X, 0);
        ts.abs(ABS_Y, 0);
        ts.key(BTN_TOUCH, true);
        ts.key(BTN_TOUCH, false);
        assert_eq!(
            frame(&mut ts),
            vec![Out::MoveTo(0.0, 0.0), Out::Press, Out::Release]
        );
    }

    #[test]
    fn sliding_while_down_drags() {
        let mut ts = screen();
        ts.key(BTN_TOUCH, true);
        ts.abs(ABS_X, 100);
        ts.abs(ABS_Y, 100);
        frame(&mut ts);
        ts.abs(ABS_X, 300);
        assert_eq!(frame(&mut ts), vec![Out::MoveTo(0.3, 0.2)]);
        // Nothing changed this frame: nothing emitted.
        assert!(frame(&mut ts).is_empty());
    }

    #[test]
    fn position_without_touch_does_nothing() {
        let mut ts = screen();
        ts.abs(ABS_X, 10);
        ts.abs(ABS_Y, 10);
        assert!(frame(&mut ts).is_empty());
    }

    #[test]
    fn press_waits_for_a_position() {
        let mut ts = screen();
        ts.key(BTN_TOUCH, true);
        assert!(frame(&mut ts).is_empty()); // no coordinates yet
        ts.abs(ABS_X, 1000);
        ts.abs(ABS_Y, 500);
        assert_eq!(frame(&mut ts), vec![Out::MoveTo(1.0, 1.0), Out::Press]);
    }

    #[test]
    fn positions_outside_the_range_are_clamped() {
        let mut ts = screen();
        ts.key(BTN_TOUCH, true);
        ts.abs(ABS_X, -50);
        ts.abs(ABS_Y, 9999);
        assert_eq!(frame(&mut ts), vec![Out::MoveTo(0.0, 1.0), Out::Press]);
    }

    #[test]
    fn release_without_press_is_ignored() {
        let mut ts = screen();
        ts.key(BTN_TOUCH, false);
        assert!(frame(&mut ts).is_empty());
    }

    #[test]
    fn touch_flags_are_swallowed_but_normal_keys_are_not() {
        let mut ts = screen();
        assert!(ts.key(0x145, true)); // BTN_TOOL_FINGER
        assert!(!ts.key(30, true)); // KEY_A
    }

    #[test]
    fn reset_forgets_the_touch() {
        let mut ts = screen();
        ts.key(BTN_TOUCH, true);
        ts.abs(ABS_X, 1);
        ts.abs(ABS_Y, 1);
        frame(&mut ts);
        ts.reset();
        assert!(!ts.is_down());
        assert!(frame(&mut ts).is_empty());
    }
}
