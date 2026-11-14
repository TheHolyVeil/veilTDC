//! Sector 1: Time-based animation clock and typed animated properties.
//!
//! # Design
//!
//! All animation state is wall-clock keyed — no frame counters. An
//! [`AnimProp<T>`] records the start `Instant`, the duration, and the
//! start/end values, then computes its current value on demand.
//!
//! [`anim_busy`] answers "is anything still animating?" so the compositor
//! tick loop can stay at `fast` rate while animations run, then back off
//! when everything settles.
//!
//! The easing function (`ease_out`) is re-exported from `veil-palette` so
//! the full codebase uses the same curve everywhere.
//!
//! # Naming conventions used in this file
//!
//! - `t` — a normalised progress value in `[0.0, 1.0]`.
//! - `prop` / `AnimProp` — an animated property holding typed start/end.
//! - `busy` — boolean: "animation is in flight, please keep the tick fast."

pub use veil_palette::ease_out;

use std::time::{Duration, Instant};

// ── Durations ────────────────────────────────────────────────────────────────

/// Map/unmap fade duration for layer surfaces that have `animate = true` for
/// their namespace (§6.1 of the design doc).
pub const FADE_MS: u64 = 120;

/// Workspace slide duration.
pub const SLIDE_MS: u64 = 180;

/// Window open (fade in + small rise) duration.
pub const WINDOW_OPEN_MS: u64 = 200;

/// How far below its place a window starts when it opens, in pixels.
pub const OPEN_SLIDE_PX: i32 = 12;

/// Vertical offset of an opening window for its current opacity: the full
/// `OPEN_SLIDE_PX` while invisible, none once fully shown. Derived from the
/// fade itself, so the rise and the fade always stay in step.
pub fn open_slide_offset(alpha: f32) -> i32 {
    ((1.0 - alpha.clamp(0.0, 1.0)) * OPEN_SLIDE_PX as f32).round() as i32
}

/// Focus-border colour-fade duration.
pub const FOCUS_FADE_MS: u64 = 150;

// ── Interpolation traits ─────────────────────────────────────────────────────

/// A type that can be linearly interpolated by a normalised `t ∈ [0, 1]`.
pub trait Lerp: Copy {
    fn lerp(a: Self, b: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    #[inline]
    fn lerp(a: f32, b: f32, t: f32) -> f32 {
        a + (b - a) * t
    }
}

impl Lerp for i32 {
    #[inline]
    fn lerp(a: i32, b: i32, t: f32) -> i32 {
        (a as f32 + (b - a) as f32 * t).round() as i32
    }
}

impl Lerp for u8 {
    #[inline]
    fn lerp(a: u8, b: u8, t: f32) -> u8 {
        (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8
    }
}

/// 3-channel RGB — used for focus-border and palette crossfades.
impl Lerp for [u8; 3] {
    #[inline]
    fn lerp(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
        [
            u8::lerp(a[0], b[0], t),
            u8::lerp(a[1], b[1], t),
            u8::lerp(a[2], b[2], t),
        ]
    }
}

/// 4-channel RGBA — used for theme colour fades.
impl Lerp for [u8; 4] {
    #[inline]
    fn lerp(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
        [
            u8::lerp(a[0], b[0], t),
            u8::lerp(a[1], b[1], t),
            u8::lerp(a[2], b[2], t),
            u8::lerp(a[3], b[3], t),
        ]
    }
}

// ── AnimProp<T> ──────────────────────────────────────────────────────────────

/// An animated property that eases from `from` to `to` over `duration`.
///
/// When `started` is `None` the property is idle and `value()` returns
/// the target (`to`) immediately. Set `started` to `Some(Instant::now())`
/// to begin an animation.
///
/// # Example
/// ```
/// use veil_host::anim::{AnimProp, FADE_MS};
/// use std::time::{Duration, Instant};
///
/// let mut alpha: AnimProp<f32> = AnimProp::immediate(1.0);
/// // Kick off a fade-out:
/// alpha.start(1.0, 0.0, Duration::from_millis(FADE_MS));
/// // Later, in the composite loop:
/// let current_alpha = alpha.value(Instant::now());
/// assert!(current_alpha >= 0.0 && current_alpha <= 1.0);
/// ```
#[derive(Debug, Clone)]
pub struct AnimProp<T: Lerp> {
    pub from: T,
    pub to: T,
    pub duration: Duration,
    pub started: Option<Instant>,
}

impl<T: Lerp> AnimProp<T> {
    /// An idle property that always returns `value`.
    pub fn immediate(value: T) -> Self {
        Self {
            from: value,
            to: value,
            duration: Duration::ZERO,
            started: None,
        }
    }

    /// Begin animating from `from` to `to` over `duration`, starting now.
    pub fn start(&mut self, from: T, to: T, duration: Duration) {
        self.from = from;
        self.to = to;
        self.duration = duration;
        self.started = Some(Instant::now());
    }

    /// Restart the same animation from the current value at `now`.
    ///
    /// Useful when reversing mid-flight (e.g. mouse leaves a surface before
    /// the fade-in finishes): snapshots the current mid-flight value as the
    /// new `from` and starts fresh.
    pub fn reverse_from_current(&mut self, new_to: T, duration: Duration, now: Instant) {
        let cur = self.value(now);
        self.start(cur, new_to, duration);
        self.started = Some(now);
    }

    /// Current eased value at `now`. Returns `to` when idle or complete.
    pub fn value(&self, now: Instant) -> T {
        let Some(started) = self.started else {
            return self.to;
        };
        let elapsed = now.duration_since(started);
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.to;
        }
        let t = ease_out(elapsed.as_secs_f32() / self.duration.as_secs_f32());
        T::lerp(self.from, self.to, t)
    }

    /// Whether the animation is still in flight at `now`.
    pub fn is_busy(&self, now: Instant) -> bool {
        match self.started {
            None => false,
            Some(started) => now.duration_since(started) < self.duration,
        }
    }

    /// Snap to the target immediately (called when animation is force-cancelled).
    pub fn finish(&mut self) {
        self.started = None;
        self.from = self.to;
    }
}

// ── Per-namespace layer surface fade state ───────────────────────────────────

/// Animation state for a single layer surface's map/unmap fade.
///
/// Created when a surface is mapped; kept until it finishes its unmap fade
/// and is fully gone. `alpha` is the animated opacity applied during blit.
#[derive(Debug, Clone)]
pub struct SurfaceFade {
    /// The current opacity [0.0, 1.0] animated over `FADE_MS`.
    pub alpha: AnimProp<f32>,
    /// True while the surface is being faded out (after `layer_destroyed`).
    pub unmapping: bool,
}

impl SurfaceFade {
    /// Start a fresh map fade (0 → 1).
    pub fn map(now: Instant) -> Self {
        Self::map_for(now, Duration::from_millis(FADE_MS))
    }

    /// Start a map fade (0 → 1) of a given length.
    pub fn map_for(now: Instant, duration: Duration) -> Self {
        let mut alpha = AnimProp::immediate(0.0);
        alpha.start(0.0, 1.0, duration);
        alpha.started = Some(now);
        Self { alpha, unmapping: false }
    }

    /// A surface that is fully shown and not animating, as the start of an
    /// unmap fade for one that never had a map fade (or whose has finished).
    pub fn shown() -> Self {
        Self { alpha: AnimProp::immediate(1.0), unmapping: false }
    }

    /// Begin an unmap fade from the current opacity down to 0.
    pub fn begin_unmap(&mut self, now: Instant) {
        let cur = self.alpha.value(now);
        self.alpha.start(cur, 0.0, Duration::from_millis(FADE_MS));
        self.alpha.started = Some(now);
        self.unmapping = true;
    }

    /// Whether the surface has fully faded out and can be removed.
    pub fn is_gone(&self, now: Instant) -> bool {
        self.unmapping && !self.alpha.is_busy(now)
    }

    /// Whether the fade is in flight.
    pub fn is_busy(&self, now: Instant) -> bool {
        self.alpha.is_busy(now)
    }
}

// ── Workspace slide ──────────────────────────────────────────────────────────

/// Per-monitor workspace slide animation.
///
/// When the user switches workspaces, `start` is called with the pixel
/// offset the outgoing workspace was drawn at (or `0`), and the target is
/// always `0` (new workspace is in place). The compositor shifts every
/// window's blit X origin by `offset.value(now)` on the outgoing side and
/// the negative of that on the incoming side.
#[derive(Debug, Clone)]
pub struct WorkspaceSlide {
    /// Current horizontal pixel offset for the transition.
    pub offset: AnimProp<i32>,
    /// The direction: +1 for "going right" (next ws), −1 for "going left".
    pub direction: i32,
}

impl WorkspaceSlide {
    pub fn idle() -> Self {
        Self {
            offset: AnimProp::immediate(0),
            direction: 0,
        }
    }

    /// Begin a slide with the given starting pixel offset.
    ///
    /// `from_offset` is the pixel displacement of the incoming workspace (e.g.
    /// `+output_width` when switching to the next workspace on the right).
    pub fn start(&mut self, from_offset: i32, now: Instant) {
        self.direction = from_offset.signum();
        self.offset.start(from_offset, 0, Duration::from_millis(SLIDE_MS));
        self.offset.started = Some(now);
    }

    pub fn is_busy(&self, now: Instant) -> bool {
        self.offset.is_busy(now)
    }

    pub fn current_offset(&self, now: Instant) -> i32 {
        self.offset.value(now)
    }
}

/// Horizontal displacement of the incoming and the outgoing workspace while a
/// slide runs, as `(incoming, outgoing)`. `incoming` is the animated offset
/// (it starts at `direction * width` and ends at 0). The outgoing workspace
/// is always exactly one screen width behind it, so the two move as one
/// strip: when the incoming one is a full width away the outgoing one is in
/// place, and once the incoming one has arrived the outgoing one is a full
/// width off the other side.
pub fn slide_offsets(incoming: i32, direction: i32, width: i32) -> (i32, i32) {
    (incoming, incoming - direction * width)
}

// ── Focus-colour fade ────────────────────────────────────────────────────────

/// Fades the focus-border colour between accent and text_dim as focus moves.
#[derive(Debug, Clone)]
pub struct FocusColorFade {
    pub color: AnimProp<[u8; 4]>,
}

impl FocusColorFade {
    pub fn immediate(color: [u8; 4]) -> Self {
        Self { color: AnimProp::immediate(color) }
    }

    /// Start fading from the current colour to `new_color`.
    pub fn fade_to(&mut self, new_color: [u8; 4], now: Instant) {
        let cur = self.color.value(now);
        self.color.start(cur, new_color, Duration::from_millis(FOCUS_FADE_MS));
        self.color.started = Some(now);
    }

    pub fn value(&self, now: Instant) -> [u8; 4] {
        self.color.value(now)
    }

    pub fn is_busy(&self, now: Instant) -> bool {
        self.color.is_busy(now)
    }
}

// ── anim_busy helper ─────────────────────────────────────────────────────────

/// Returns `true` if any of the supplied [`AnimProp`] / animation objects are
/// still running at `now`. Keeps the compositor tick at `fast` rate while
/// any animation is in flight.
///
/// Usage in the event loop:
/// ```ignore
/// busy |= anim_busy(now, [
///     state.ws_slide.is_busy(now),
///     state.focus_fade.is_busy(now),
///     // …
/// ]);
/// ```
#[inline]
pub fn anim_busy(conditions: impl IntoIterator<Item = bool>) -> bool {
    conditions.into_iter().any(|b| b)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ── Lerp correctness ────────────────────────────────────────────────────

    #[test]
    fn lerp_f32_endpoints() {
        assert_eq!(f32::lerp(0.0, 10.0, 0.0), 0.0);
        assert_eq!(f32::lerp(0.0, 10.0, 1.0), 10.0);
    }

    #[test]
    fn lerp_i32_midpoint() {
        // 0 → 100 at t=0.5 → 50
        assert_eq!(i32::lerp(0, 100, 0.5), 50);
    }

    #[test]
    fn open_slide_follows_the_fade() {
        assert_eq!(open_slide_offset(0.0), OPEN_SLIDE_PX);
        assert_eq!(open_slide_offset(1.0), 0);
        assert_eq!(open_slide_offset(0.5), OPEN_SLIDE_PX / 2);
        // Out-of-range alpha never pushes a window further than its start
        // point or past its place.
        assert_eq!(open_slide_offset(-3.0), OPEN_SLIDE_PX);
        assert_eq!(open_slide_offset(7.0), 0);
        // Monotonic: the window only ever rises.
        let mut last = i32::MAX;
        for step in 0..=20 {
            let o = open_slide_offset(step as f32 / 20.0);
            assert!(o <= last);
            last = o;
        }
    }

    #[test]
    fn a_shown_surface_unmaps_to_gone() {
        let t = Instant::now();
        let mut f = SurfaceFade::shown();
        assert!(!f.is_busy(t) && !f.is_gone(t));
        assert_eq!(f.alpha.value(t), 1.0);
        f.begin_unmap(t);
        let mid = t + Duration::from_millis(FADE_MS / 2);
        assert!(f.is_busy(mid) && !f.is_gone(mid));
        assert!(f.alpha.value(mid) < 1.0 && f.alpha.value(mid) > 0.0);
        let end = t + Duration::from_millis(FADE_MS + 1);
        assert!(f.is_gone(end));
        assert_eq!(f.alpha.value(end), 0.0);
    }

    #[test]
    fn map_for_runs_for_its_own_duration() {
        let t = Instant::now();
        let f = SurfaceFade::map_for(t, Duration::from_millis(WINDOW_OPEN_MS));
        assert!(f.is_busy(t + Duration::from_millis(WINDOW_OPEN_MS - 1)));
        assert!(!f.is_busy(t + Duration::from_millis(WINDOW_OPEN_MS)));
        assert_eq!(f.alpha.value(t), 0.0);
        assert_eq!(f.alpha.value(t + Duration::from_millis(WINDOW_OPEN_MS)), 1.0);
        assert!(!f.unmapping);
    }

    #[test]
    fn lerp_u8_clamps() {
        // Should never overflow
        assert_eq!(u8::lerp(200, 255, 0.5), 228);
        assert_eq!(u8::lerp(0, 0, 0.5), 0);
    }

    #[test]
    fn lerp_rgb_endpoints() {
        let a = [0u8, 0, 0];
        let b = [255u8, 128, 64];
        assert_eq!(<[u8; 3]>::lerp(a, b, 0.0), a);
        assert_eq!(<[u8; 3]>::lerp(a, b, 1.0), b);
    }

    // ── AnimProp ────────────────────────────────────────────────────────────

    #[test]
    fn immediate_prop_returns_target() {
        let p: AnimProp<f32> = AnimProp::immediate(0.75);
        let now = Instant::now();
        assert_eq!(p.value(now), 0.75);
        assert!(!p.is_busy(now));
    }

    #[test]
    fn prop_at_start_returns_from() {
        let mut p: AnimProp<f32> = AnimProp::immediate(0.0);
        let t0 = Instant::now();
        p.start(0.0, 1.0, Duration::from_millis(100));
        p.started = Some(t0);
        // At exactly t0 elapsed ≈ 0 → eased progress ≈ 0 → value ≈ 0.0
        let v = p.value(t0);
        assert!(v < 0.1, "value at t=0 should be near from, got {v}");
    }

    #[test]
    fn prop_after_duration_returns_to() {
        let mut p: AnimProp<f32> = AnimProp::immediate(0.0);
        let t0 = Instant::now() - Duration::from_millis(200);
        p.start(0.0, 1.0, Duration::from_millis(100));
        p.started = Some(t0);
        assert_eq!(p.value(Instant::now()), 1.0);
        assert!(!p.is_busy(Instant::now()));
    }

    #[test]
    fn prop_midway_is_between_from_and_to() {
        let mut p: AnimProp<f32> = AnimProp::immediate(0.0);
        let dur = Duration::from_millis(200);
        // Start 100 ms ago → halfway through
        let t0 = Instant::now() - Duration::from_millis(100);
        p.start(0.0, 1.0, dur);
        p.started = Some(t0);
        let v = p.value(Instant::now());
        // ease_out at t=0.5 is > 0.5 (ease-out leads), so just check in range
        assert!(v > 0.0 && v < 1.0, "midway value out of range: {v}");
    }

    #[test]
    fn prop_busy_while_in_flight() {
        let mut p: AnimProp<f32> = AnimProp::immediate(0.0);
        let t0 = Instant::now();
        p.start(0.0, 1.0, Duration::from_millis(500));
        p.started = Some(t0);
        assert!(p.is_busy(t0 + Duration::from_millis(100)));
        assert!(!p.is_busy(t0 + Duration::from_millis(600)));
    }

    #[test]
    fn prop_finish_snaps_to_target() {
        let mut p: AnimProp<f32> = AnimProp::immediate(0.0);
        let t0 = Instant::now();
        p.start(0.0, 1.0, Duration::from_millis(500));
        p.started = Some(t0);
        p.finish();
        assert_eq!(p.value(t0 + Duration::from_millis(10)), 1.0);
        assert!(!p.is_busy(t0 + Duration::from_millis(10)));
    }

    // ── SurfaceFade ─────────────────────────────────────────────────────────

    #[test]
    fn surface_fade_map_starts_at_zero() {
        let now = Instant::now();
        let fade = SurfaceFade::map(now);
        let v = fade.alpha.value(now);
        assert!(v < 0.1, "map fade should start near 0, got {v}");
        assert!(fade.alpha.is_busy(now));
        assert!(!fade.unmapping);
    }

    #[test]
    fn surface_fade_map_reaches_one() {
        let t0 = Instant::now() - Duration::from_millis(FADE_MS + 50);
        let fade = SurfaceFade::map(t0);
        assert_eq!(fade.alpha.value(Instant::now()), 1.0);
        assert!(!fade.is_busy(Instant::now()));
    }

    #[test]
    fn surface_fade_unmap_goes_to_zero() {
        let now = Instant::now();
        let mut fade = SurfaceFade::map(now - Duration::from_millis(FADE_MS + 50));
        assert_eq!(fade.alpha.value(now), 1.0);
        fade.begin_unmap(now);
        assert!(fade.unmapping);
        // After full FADE_MS it should be done and report gone
        let later = now + Duration::from_millis(FADE_MS + 10);
        assert!(fade.is_gone(later));
    }

    // ── WorkspaceSlide ───────────────────────────────────────────────────────

    #[test]
    fn workspace_slide_idle_offset_is_zero() {
        let ws = WorkspaceSlide::idle();
        assert_eq!(ws.current_offset(Instant::now()), 0);
        assert!(!ws.is_busy(Instant::now()));
    }

    #[test]
    fn workspace_slide_reaches_zero() {
        let now = Instant::now() - Duration::from_millis(SLIDE_MS + 50);
        let mut ws = WorkspaceSlide::idle();
        ws.start(1920, now);
        assert_eq!(ws.current_offset(Instant::now()), 0);
        assert!(!ws.is_busy(Instant::now()));
    }

    // ── FocusColorFade ──────────────────────────────────────────────────────

    #[test]
    fn slide_offsets_move_as_one_strip() {
        let w = 1920;
        // Going right: the new workspace starts a screen to the right...
        assert_eq!(slide_offsets(w, 1, w), (w, 0));
        // ...halfway, the old one has gone half a screen left...
        assert_eq!(slide_offsets(w / 2, 1, w), (w / 2, -w / 2));
        // ...and on arrival the old one is a full screen off to the left.
        assert_eq!(slide_offsets(0, 1, w), (0, -w));
        // Going left mirrors it.
        assert_eq!(slide_offsets(-w, -1, w), (-w, 0));
        assert_eq!(slide_offsets(0, -1, w), (0, w));
        // The gap between the two never changes.
        for inc in [-1920, -700, 0, 300, 1920] {
            let dir = if inc >= 0 { 1 } else { -1 };
            let (i, o) = slide_offsets(inc, dir, w);
            assert_eq!(i - o, dir * w);
        }
    }

    #[test]
    fn focus_fade_immediate_returns_color() {
        let c = [255u8, 0, 128, 255];
        let f = FocusColorFade::immediate(c);
        assert_eq!(f.value(Instant::now()), c);
    }

    #[test]
    fn focus_fade_reaches_target() {
        let start = [0u8, 0, 0, 255];
        let end = [255u8, 128, 64, 255];
        let mut f = FocusColorFade::immediate(start);
        let t0 = Instant::now() - Duration::from_millis(FOCUS_FADE_MS + 50);
        f.fade_to(end, t0);
        assert_eq!(f.value(Instant::now()), end);
    }

    // ── anim_busy ───────────────────────────────────────────────────────────

    #[test]
    fn anim_busy_true_when_any_busy() {
        assert!(anim_busy([false, true, false]));
    }

    #[test]
    fn anim_busy_false_when_all_idle() {
        assert!(!anim_busy([false, false, false]));
    }

    // ── Easing (re-exported from veil-palette) ───────────────────────────────

    #[test]
    fn ease_out_endpoints() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
    }

    #[test]
    fn ease_out_leads_at_midpoint() {
        // ease-out cubic: fast start, slow end → at t=0.5 output > 0.5
        assert!(ease_out(0.5) > 0.5, "ease-out should lead at the midpoint");
    }

    #[test]
    fn ease_out_is_monotonic() {
        let mut prev = 0.0_f32;
        for i in 1..=20u32 {
            let v = ease_out(i as f32 / 20.0);
            assert!(v >= prev, "ease_out not monotonic at i={i}");
            prev = v;
        }
    }
}
