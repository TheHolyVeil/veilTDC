//! Touchscreen support for the bare-TTY evdev backend.
//!
//! A touchscreen reports absolute positions (ABS_X/ABS_Y, device units) and
//! BTN_TOUCH. The first finger is turned into pointer input by what it does
//! (the kernel's single-touch emulation hands us that finger):
//!
//! * **tap**: a short touch that barely moves is a left click at that spot.
//! * **drag**: sliding past the slop scrolls the page, content following the
//!   finger (drag down shows what is above). Lifting a fast flick keeps the
//!   page gliding and slowing down; touching the screen again stops it, and
//!   that touch does not click.
//! * **long press**: holding still shows intent. Lifting without moving is a
//!   right click; sliding instead starts a left-button drag (select text,
//!   move a window) from where the finger first landed.
//!
//! No multi-touch. Pure and unit-tested: the caller passes the time and the
//! output size in pixels, calls `sync` on SYN_REPORT, and calls `tick`
//! whenever `next_deadline` says a timer is due.

use std::time::{Duration, Instant};

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const BTN_TOUCH: u16 = 0x14a;

/// Finger travel that still counts as "not moved" (a tap, or a held finger).
const SLOP_PX: f32 = 12.0;
/// Hold this long without moving to arm right-click / drag.
const LONG_PRESS: Duration = Duration::from_millis(450);
/// Finger travel that scrolls one wheel notch (v120 = 120). Slightly under
/// the height of a notch in common clients, so content keeps up with the
/// finger instead of lagging behind it.
const PX_PER_NOTCH: f32 = 48.0;
const V120_PER_PX: f32 = 120.0 / PX_PER_NOTCH;
/// A flick must leave the screen faster than this to glide (px/s)...
const FLING_MIN_PX_S: f32 = 150.0;
/// ...and must still have been moving this recently when it lifted.
const FLING_STALE: Duration = Duration::from_millis(80);
/// Cap on the release velocity (px/s), so a glitchy sample cannot launch a
/// page into orbit.
const FLING_MAX_PX_S: f32 = 6000.0;
/// Glide slows by 1/e every this many seconds.
const FLING_TAU: f32 = 0.40;
/// The glide ends below this speed (v120 per second, a quarter notch/s).
const FLING_STOP_V120_S: f32 = 30.0;
/// How often a gliding page is advanced.
const FLING_TICK: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Out {
    /// Pointer position as a fraction of the output, each in `0.0..=1.0`.
    MoveTo(f32, f32),
    /// Left button down.
    Press,
    /// Left button up.
    Release,
    /// A whole right click (press and release).
    RightClick,
    /// Vertical scroll in v120 units; negative = content moves down (same
    /// convention as the touchpad's natural scrolling).
    Scroll(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Down, still undecided: a tap, a scroll or a long press.
    Pending,
    /// Held still past `LONG_PRESS`: lifting is a right click, sliding drags.
    Held,
    Scrolling,
    /// Left button is held and the finger drags it.
    Dragging,
}

#[derive(Clone, Copy, Debug)]
struct Fling {
    /// Current speed in v120 per second (sign as in `Out::Scroll`).
    v120_s: f32,
    last: Instant,
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
    pending_down: bool,
    pending_up: bool,
    moved: bool,

    phase: Phase,
    down_at: Option<Instant>,
    /// Where the finger landed: pixels (for the slop test) and the fraction
    /// (to start a drag from there).
    start_px: (f32, f32),
    start_frac: (f32, f32),
    /// Last finger position seen while scrolling, and when.
    last_px: (f32, f32),
    last_move: Option<Instant>,
    /// Smoothed finger velocity while scrolling (px/s, + = down).
    vel: f32,
    /// Sub-notch scroll carried between emissions.
    accum: f32,
    fling: Option<Fling>,
    /// This touch stopped a glide, so lifting it must not click.
    suppress_tap: bool,
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
            pending_down: false,
            pending_up: false,
            moved: false,
            phase: Phase::Idle,
            down_at: None,
            start_px: (0.0, 0.0),
            start_frac: (0.0, 0.0),
            last_px: (0.0, 0.0),
            last_move: None,
            vel: 0.0,
            accum: 0.0,
            fling: None,
            suppress_tap: false,
        }
    }

    /// Drop any touch in progress (session went inactive). The caller is
    /// responsible for having released the button if `is_down`.
    pub fn reset(&mut self) {
        *self = Self::new((self.min_x, self.max_x), (self.min_y, self.max_y));
    }

    /// Whether the left button is currently held (so the caller can release
    /// it): only a long-press drag holds it.
    pub fn is_down(&self) -> bool {
        self.phase == Phase::Dragging
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

    /// Queue `v120` of scroll, emitting whole units and carrying the rest.
    fn scroll(&mut self, v120: f32, out: &mut Vec<Out>) {
        self.accum += v120;
        let whole = self.accum.trunc();
        if whole != 0.0 {
            self.accum -= whole;
            out.push(Out::Scroll(whole as i32));
        }
    }

    /// SYN_REPORT: emit what this frame amounts to. `size` is the output in
    /// pixels (slop and scroll distance are measured on screen, not in
    /// device units).
    pub fn sync(&mut self, now: Instant, size: (u32, u32), out: &mut Vec<Out>) {
        let have_pos = self.have_x && self.have_y;
        let frac = self.fraction();
        // Hundredths of a pixel are plenty, and they keep float noise from
        // turning a 48.0 px drag into 47.99998.
        let round2 = |v: f32| (v * 100.0).round() / 100.0;
        let px = (
            round2(frac.0 * size.0.max(1) as f32),
            round2(frac.1 * size.1.max(1) as f32),
        );

        if self.pending_down && have_pos {
            self.pending_down = false;
            // A touch landing on a gliding page stops it and is not a click.
            self.suppress_tap = self.fling.take().is_some();
            self.accum = 0.0;
            self.phase = Phase::Pending;
            self.down_at = Some(now);
            self.start_px = px;
            self.start_frac = frac;
            self.last_px = px;
            self.last_move = Some(now);
            self.vel = 0.0;
            // The pointer goes under the finger now, so wheel events and the
            // eventual click both land there.
            out.push(Out::MoveTo(frac.0, frac.1));
        } else if self.moved && have_pos {
            self.motion(now, px, frac, out);
        }
        self.moved = false;

        if self.pending_up {
            self.release(now, out);
            self.pending_up = false;
            // A touch that ended before we ever saw a position is dropped.
            self.pending_down = false;
        }
    }

    fn dist_from_start(&self, px: (f32, f32)) -> f32 {
        let (dx, dy) = (px.0 - self.start_px.0, px.1 - self.start_px.1);
        (dx * dx + dy * dy).sqrt()
    }

    fn motion(&mut self, now: Instant, px: (f32, f32), frac: (f32, f32), out: &mut Vec<Out>) {
        match self.phase {
            Phase::Idle => {}
            Phase::Pending => {
                if self.dist_from_start(px) > SLOP_PX {
                    // Start scrolling from here: no jump for the slop already
                    // travelled.
                    self.phase = Phase::Scrolling;
                    self.last_px = px;
                    self.last_move = Some(now);
                    self.vel = 0.0;
                }
            }
            Phase::Held => {
                if self.dist_from_start(px) > SLOP_PX {
                    // Drag from where the finger first landed.
                    out.push(Out::MoveTo(self.start_frac.0, self.start_frac.1));
                    out.push(Out::Press);
                    out.push(Out::MoveTo(frac.0, frac.1));
                    self.phase = Phase::Dragging;
                }
            }
            Phase::Scrolling => {
                let dy = px.1 - self.last_px.1;
                let dt = now
                    .saturating_duration_since(self.last_move.unwrap_or(now))
                    .as_secs_f32()
                    .max(0.001);
                let inst = dy / dt;
                self.vel = if dt > 0.1 { inst } else { 0.5 * self.vel + 0.5 * inst };
                self.scroll(-dy * V120_PER_PX, out);
                self.last_px = px;
                self.last_move = Some(now);
            }
            Phase::Dragging => out.push(Out::MoveTo(frac.0, frac.1)),
        }
    }

    fn release(&mut self, now: Instant, out: &mut Vec<Out>) {
        // Robust against a late timer: held long enough is a long press.
        if self.phase == Phase::Pending
            && self.down_at.is_some_and(|t| now.saturating_duration_since(t) >= LONG_PRESS)
        {
            self.phase = Phase::Held;
        }
        match self.phase {
            Phase::Pending => {
                if !self.suppress_tap {
                    out.push(Out::Press);
                    out.push(Out::Release);
                }
            }
            Phase::Held => out.push(Out::RightClick),
            Phase::Scrolling => {
                let fresh = self
                    .last_move
                    .is_some_and(|t| now.saturating_duration_since(t) <= FLING_STALE);
                if fresh && self.vel.abs() >= FLING_MIN_PX_S {
                    let v = self.vel.clamp(-FLING_MAX_PX_S, FLING_MAX_PX_S);
                    self.fling = Some(Fling { v120_s: -v * V120_PER_PX, last: now });
                }
            }
            Phase::Dragging => out.push(Out::Release),
            Phase::Idle => {}
        }
        self.phase = Phase::Idle;
        self.down_at = None;
        self.suppress_tap = false;
    }

    /// Advance timers: arm a long press, and move a gliding page along.
    pub fn tick(&mut self, now: Instant, out: &mut Vec<Out>) {
        if self.phase == Phase::Pending
            && self.down_at.is_some_and(|t| now.saturating_duration_since(t) >= LONG_PRESS)
        {
            self.phase = Phase::Held;
        }
        if let Some(f) = self.fling {
            let dt = now.saturating_duration_since(f.last).as_secs_f32();
            if dt > 0.0 {
                // Exact integral of an exponentially decaying speed over dt.
                let decay = (-dt / FLING_TAU).exp();
                self.scroll(f.v120_s * FLING_TAU * (1.0 - decay), out);
                let v = f.v120_s * decay;
                if v.abs() < FLING_STOP_V120_S {
                    self.fling = None;
                    self.accum = 0.0;
                } else {
                    self.fling = Some(Fling { v120_s: v, last: now });
                }
            }
        }
    }

    /// How long until `tick` has something to do, if anything is pending.
    /// The caller uses it as its poll timeout.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        let mut d: Option<Duration> = None;
        if self.phase == Phase::Pending {
            if let Some(t) = self.down_at {
                d = Some((t + LONG_PRESS).saturating_duration_since(now));
            }
        }
        if self.fling.is_some() {
            d = Some(d.map_or(FLING_TICK, |x| x.min(FLING_TICK)));
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Output the tests pretend to have; the device range is 1000 x 500 so
    /// one device unit is exactly one pixel.
    const SIZE: (u32, u32) = (1000, 500);

    fn screen() -> Touchscreen {
        Touchscreen::new((0, 1000), (0, 500))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn frame(ts: &mut Touchscreen, t: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        ts.sync(t, SIZE, &mut out);
        out
    }

    fn tick(ts: &mut Touchscreen, t: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        ts.tick(t, &mut out);
        out
    }

    fn touch_down(ts: &mut Touchscreen, t: Instant, x: i32, y: i32) -> Vec<Out> {
        ts.key(BTN_TOUCH, true);
        ts.abs(ABS_X, x);
        ts.abs(ABS_Y, y);
        frame(ts, t)
    }

    fn move_to(ts: &mut Touchscreen, t: Instant, x: i32, y: i32) -> Vec<Out> {
        ts.abs(ABS_X, x);
        ts.abs(ABS_Y, y);
        frame(ts, t)
    }

    fn lift(ts: &mut Touchscreen, t: Instant) -> Vec<Out> {
        ts.key(BTN_TOUCH, false);
        frame(ts, t)
    }

    fn scrolled(out: &[Out]) -> i32 {
        out.iter()
            .map(|o| if let Out::Scroll(v) = o { *v } else { 0 })
            .sum()
    }

    #[test]
    fn touching_moves_the_pointer_but_does_not_click_yet() {
        let mut ts = screen();
        let t = Instant::now();
        assert_eq!(touch_down(&mut ts, t, 500, 250), vec![Out::MoveTo(0.5, 0.5)]);
        assert!(!ts.is_down());
    }

    #[test]
    fn a_short_touch_is_a_click() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 250);
        assert_eq!(lift(&mut ts, t + ms(90)), vec![Out::Press, Out::Release]);
    }

    #[test]
    fn very_quick_tap_in_one_frame_still_clicks() {
        let mut ts = screen();
        let t = Instant::now();
        ts.abs(ABS_X, 0);
        ts.abs(ABS_Y, 0);
        ts.key(BTN_TOUCH, true);
        ts.key(BTN_TOUCH, false);
        assert_eq!(
            frame(&mut ts, t),
            vec![Out::MoveTo(0.0, 0.0), Out::Press, Out::Release]
        );
    }

    #[test]
    fn jitter_inside_the_slop_is_still_a_tap() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 250);
        assert!(move_to(&mut ts, t + ms(20), 505, 254).is_empty());
        assert!(move_to(&mut ts, t + ms(40), 498, 247).is_empty());
        assert_eq!(lift(&mut ts, t + ms(80)), vec![Out::Press, Out::Release]);
    }

    #[test]
    fn dragging_down_scrolls_up_like_a_natural_scroll() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 100);
        // Past the slop: scrolling starts, nothing is emitted for that move.
        assert!(move_to(&mut ts, t + ms(30), 500, 130).is_empty());
        // 48 px of further finger travel is one notch, content following the
        // finger down means a negative (up) scroll.
        let out = move_to(&mut ts, t + ms(60), 500, 178);
        assert_eq!(scrolled(&out), -120);
        // Dragging back up scrolls the other way.
        let out = move_to(&mut ts, t + ms(90), 500, 130);
        assert_eq!(scrolled(&out), 120);
    }

    #[test]
    fn scrolling_never_clicks_and_never_presses() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 100);
        move_to(&mut ts, t + ms(30), 500, 160);
        move_to(&mut ts, t + ms(500), 500, 161); // slow, so no glide
        let out = lift(&mut ts, t + ms(900));
        assert!(!out.contains(&Out::Press) && !out.contains(&Out::Release));
        assert!(!ts.is_down());
    }

    #[test]
    fn small_moves_carry_their_remainder() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 100);
        move_to(&mut ts, t + ms(20), 500, 130); // starts scrolling
        let mut total = 0;
        // 40 one-pixel steps, 100 ms apart (too slow to glide) = 40 px.
        for i in 1..=40 {
            let out = move_to(&mut ts, t + ms(20 + 100 * i), 500, 130 + i as i32);
            total += scrolled(&out);
        }
        // 40 px * 2.5 v120/px = 100, none lost to rounding (+- the remainder).
        assert_eq!(total, -100);
    }

    #[test]
    fn a_fast_flick_keeps_gliding_and_then_stops() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 100);
        move_to(&mut ts, t + ms(10), 500, 120);
        move_to(&mut ts, t + ms(20), 500, 150);
        move_to(&mut ts, t + ms(30), 500, 190);
        move_to(&mut ts, t + ms(40), 500, 240); // ~5000 px/s
        assert!(lift(&mut ts, t + ms(45)).is_empty());
        assert!(ts.next_deadline(t + ms(45)).is_some(), "glide needs ticks");

        let mut now = t + ms(45);
        let mut total = 0;
        let mut first_chunk = 0;
        for i in 0..400 {
            now += ms(8);
            let out = tick(&mut ts, now);
            total += scrolled(&out);
            if i == 0 {
                first_chunk = scrolled(&out);
            }
        }
        assert!(total < -120, "the page should travel several notches, got {total}");
        assert!(first_chunk <= 0, "gliding continues the drag direction");
        // After a few seconds it has slowed to a halt and needs no more ticks.
        assert!(ts.next_deadline(now).is_none());
        assert!(tick(&mut ts, now + ms(8)).is_empty());
    }

    #[test]
    fn a_slow_release_does_not_glide() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 100);
        move_to(&mut ts, t + ms(20), 500, 130);
        move_to(&mut ts, t + ms(400), 500, 140); // ~25 px/s
        lift(&mut ts, t + ms(420));
        assert!(ts.next_deadline(t + ms(420)).is_none());
    }

    #[test]
    fn resting_before_lifting_does_not_glide() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 100);
        move_to(&mut ts, t + ms(10), 500, 140);
        move_to(&mut ts, t + ms(20), 500, 200);
        move_to(&mut ts, t + ms(30), 500, 300); // fast...
        lift(&mut ts, t + ms(500)); // ...but the finger rested a long time
        assert!(ts.next_deadline(t + ms(500)).is_none());
    }

    #[test]
    fn touching_a_gliding_page_stops_it_without_clicking() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 500, 100);
        move_to(&mut ts, t + ms(10), 500, 150);
        move_to(&mut ts, t + ms(20), 500, 220);
        move_to(&mut ts, t + ms(30), 500, 300);
        lift(&mut ts, t + ms(35));
        assert!(ts.next_deadline(t + ms(35)).is_some());

        let t2 = t + ms(120);
        touch_down(&mut ts, t2, 400, 300);
        assert!(ts.next_deadline(t2 + ms(1)).is_none_or(|d| d > FLING_TICK), "glide stopped");
        assert!(tick(&mut ts, t2 + ms(16)).is_empty());
        // That tap only stopped the page.
        assert!(lift(&mut ts, t2 + ms(60)).is_empty());
        // The next one clicks again.
        let t3 = t2 + ms(400);
        touch_down(&mut ts, t3, 400, 300);
        assert_eq!(lift(&mut ts, t3 + ms(60)), vec![Out::Press, Out::Release]);
    }

    #[test]
    fn long_press_then_lift_is_a_right_click() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 300, 200);
        assert_eq!(ts.next_deadline(t), Some(LONG_PRESS));
        assert!(tick(&mut ts, t + ms(460)).is_empty());
        assert_eq!(lift(&mut ts, t + ms(600)), vec![Out::RightClick]);
    }

    #[test]
    fn long_press_is_recognised_at_lift_even_if_the_timer_was_late() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 300, 200);
        assert_eq!(lift(&mut ts, t + ms(700)), vec![Out::RightClick]);
    }

    #[test]
    fn long_press_then_slide_drags_from_where_the_finger_landed() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 100, 100);
        tick(&mut ts, t + ms(460));
        // Inside the slop still: nothing yet.
        assert!(move_to(&mut ts, t + ms(500), 105, 100).is_empty());
        assert!(!ts.is_down());
        // Past it: start of the drag, anchored at the landing point.
        let out = move_to(&mut ts, t + ms(540), 160, 100);
        assert_eq!(
            out,
            vec![Out::MoveTo(0.1, 0.2), Out::Press, Out::MoveTo(0.16, 0.2)]
        );
        assert!(ts.is_down());
        assert_eq!(move_to(&mut ts, t + ms(560), 200, 120), vec![Out::MoveTo(0.2, 0.24)]);
        assert_eq!(lift(&mut ts, t + ms(600)), vec![Out::Release]);
        assert!(!ts.is_down());
    }

    #[test]
    fn moving_before_the_long_press_scrolls_instead() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 100, 100);
        move_to(&mut ts, t + ms(100), 100, 140); // scrolling now
        tick(&mut ts, t + ms(600)); // timer no longer applies
        assert!(!ts.is_down());
        assert!(ts.next_deadline(t + ms(600)).is_none());
    }

    #[test]
    fn position_without_touch_does_nothing() {
        let mut ts = screen();
        let t = Instant::now();
        ts.abs(ABS_X, 10);
        ts.abs(ABS_Y, 10);
        assert!(frame(&mut ts, t).is_empty());
    }

    #[test]
    fn touch_waits_for_a_position() {
        let mut ts = screen();
        let t = Instant::now();
        ts.key(BTN_TOUCH, true);
        assert!(frame(&mut ts, t).is_empty()); // no coordinates yet
        ts.abs(ABS_X, 1000);
        ts.abs(ABS_Y, 500);
        assert_eq!(frame(&mut ts, t + ms(5)), vec![Out::MoveTo(1.0, 1.0)]);
    }

    #[test]
    fn positions_outside_the_range_are_clamped() {
        let mut ts = screen();
        let t = Instant::now();
        ts.key(BTN_TOUCH, true);
        ts.abs(ABS_X, -50);
        ts.abs(ABS_Y, 9999);
        assert_eq!(frame(&mut ts, t), vec![Out::MoveTo(0.0, 1.0)]);
    }

    #[test]
    fn release_without_touch_is_ignored() {
        let mut ts = screen();
        let t = Instant::now();
        ts.key(BTN_TOUCH, false);
        assert!(frame(&mut ts, t).is_empty());
    }

    #[test]
    fn touch_flags_are_swallowed_but_normal_keys_are_not() {
        let mut ts = screen();
        assert!(ts.key(0x145, true)); // BTN_TOOL_FINGER
        assert!(!ts.key(30, true)); // KEY_A
    }

    #[test]
    fn reset_forgets_the_touch_and_any_glide() {
        let mut ts = screen();
        let t = Instant::now();
        touch_down(&mut ts, t, 100, 100);
        tick(&mut ts, t + ms(460));
        move_to(&mut ts, t + ms(500), 300, 100); // dragging
        assert!(ts.is_down());
        ts.reset();
        assert!(!ts.is_down());
        assert!(ts.next_deadline(t + ms(600)).is_none());
        assert!(frame(&mut ts, t + ms(600)).is_empty());
    }
}
