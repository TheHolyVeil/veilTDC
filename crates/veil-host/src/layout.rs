//! Dwindle tiling layout — Combo 3.
//!
//! veil-host tiles up to four toplevels. The window *order* is owned by
//! `State.toplevels`; this module owns only what can't be derived from that
//! list — which window has focus, and whether the primary split is flipped —
//! plus the pure geometry that turns `(count, width, height)` into rects.
//!
//! The dwindle progression (capped at 4, per the v2.0 spec):
//!   1 → fullscreen
//!   2 → left / right split          (flip: top / bottom)
//!   3 → left half + right column halved   (flip: top half + bottom row halved)
//!   4 → 2×2 grid
//! A 5th+ window stacks on the last cell (we cap at 4 visually).

/// How far left parked (zoomed-away) windows sit; bigger than any panel.
const PARK_OFFSET: i32 = 100_000;

/// A window rectangle in compositor pixel space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    /// Shrink by `d` px on every side (never below 1x1).
    pub fn inset(&self, d: i32) -> Rect {
        let dd = d.max(0) as u32;
        Rect {
            x: self.x + d,
            y: self.y + d,
            w: self.w.saturating_sub(2 * dd).max(1),
            h: self.h.saturating_sub(2 * dd).max(1),
        }
    }

    /// Grow by `d` px on every side.
    pub fn inflate(&self, d: u32) -> Rect {
        Rect {
            x: self.x - d as i32,
            y: self.y - d as i32,
            w: self.w + 2 * d,
            h: self.h + 2 * d,
        }
    }

    fn center(&self) -> (i32, i32) {
        (self.x + self.w as i32 / 2, self.y + self.h as i32 / 2)
    }

    /// Whether the point `(x, y)` falls within this rect — right/bottom
    /// edges excluded, matching every other hit-test in this file (click
    /// focus, popup bounds). Used for pointer-to-monitor routing: which
    /// `Monitor::rect` the shared virtual pointer position currently falls
    /// inside.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w as i32 && y < self.y + self.h as i32
    }

    /// Smallest rect containing both `self` and `other`. Used to accumulate
    /// damage: each changed region gets unioned into a running bounding box
    /// rather than tracked as an exact list, so composite only needs to
    /// carry one rect per frame instead of an unbounded set.
    pub fn union(&self, other: &Rect) -> Rect {
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = (self.x + self.w as i32).max(other.x + other.w as i32);
        let y1 = (self.y + self.h as i32).max(other.y + other.h as i32);
        Rect {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0) as u32,
            h: (y1 - y0).max(0) as u32,
        }
    }

    /// Clip to `[0,0]..[max_w,max_h]` — defensive against a stale damage
    /// rect referencing dimensions from before an output resize.
    pub fn clamp_to(&self, max_w: u32, max_h: u32) -> Rect {
        let x0 = self.x.max(0).min(max_w as i32);
        let y0 = self.y.max(0).min(max_h as i32);
        let x1 = (self.x + self.w as i32).max(0).min(max_w as i32);
        let y1 = (self.y + self.h as i32).max(0).min(max_h as i32);
        Rect {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0) as u32,
            h: (y1 - y0).max(0) as u32,
        }
    }
}

/// Direction for spatial focus movement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// `split_ratio` bounds — never let the primary pane shrink to nothing or
/// swallow the whole output.
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;
/// Per-keypress adjustment for `resize_grow`/`resize_shrink`.
const RESIZE_STEP: f32 = 0.05;

/// Which tiling algorithm computes rects for the current window set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    /// v2.0 behavior — 1..4 windows via recursive splits, 5+ stacks on the
    /// last cell (unusable past 4). Default, unchanged.
    Dwindle,
    /// Horizontal scrolling strip (PaperWM/niri-style). Every window gets a
    /// fixed-width column; no cap on window count — excess windows scroll
    /// off-screen instead of shrinking. No separate scroll-position field:
    /// the viewport is always derived from `focused`, the same way dwindle's
    /// rects are always derived from `flip`/`split_ratio` rather than
    /// cached — one less piece of state that can drift out of sync.
    Scroll,
}

/// Tiling state not derivable from the window list.
#[derive(Clone, Copy)]
pub struct Layout {
    /// Index (into the live-toplevel order) of the focused window.
    pub focused: usize,
    /// `rotate_split` toggles this — flips the primary split axis. Unused
    /// in `Scroll` mode (no axis to flip in a linear strip); harmless no-op
    /// there, not worth special-casing.
    pub flip: bool,
    /// Primary pane's share of the OUTERMOST split only — adjusted by
    /// `resize_grow`/`resize_shrink` (Super+=/-). The secondary split (the
    /// stack's internal top/bottom for n=3, the whole n=4 grid) always stays
    /// a fixed 50/50; this is dwm-style "mfact", not per-pane resizing.
    /// In `Scroll` mode this same field/keybinds double as column width
    /// instead — one ratio, one pair of keybinds, two meanings depending on
    /// mode, rather than a second ratio field.
    pub split_ratio: f32,
    /// Which algorithm `rects()` uses. `toggle_mode()` flips it.
    pub mode: LayoutMode,
    /// Scroll mode only: x of the viewport's left edge in strip space. This
    /// IS state now (it used to be derived from `focused`, which made every
    /// focus change — including a mouse click — snap the clicked column to
    /// the left edge). Moved only by `sync_view`, and only when the focused
    /// column would otherwise be off-screen.
    pub view_x: i32,
    /// Set by click-to-focus so the next `sync_view` doesn't scroll to the
    /// clicked column. Consumed (cleared) by `sync_view`.
    pub skip_reveal: bool,
    /// "Fake fullscreen": the focused window fills the whole work area
    /// (bar/layer-shell zones stay visible, no xdg Fullscreen state sent).
    /// Follows `focused`; every other window is parked off-screen at its
    /// normal size so toggling doesn't resize them.
    pub zoomed: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            focused: 0,
            flip: false,
            split_ratio: 0.5,
            mode: LayoutMode::Dwindle,
            view_x: 0,
            skip_reveal: false,
            zoomed: false,
        }
    }
}

impl Layout {
    /// Compute a rect for each of `n` windows filling `w`×`h`. Dispatches on
    /// `self.mode`; see `rects_dwindle`/`rects_scroll`. Scroll mode reads
    /// `view_x`, so call `sync_view` first whenever `focused`/`n`/`w` may
    /// have changed.
    pub fn rects(&self, n: usize, w: u32, h: u32) -> Vec<Rect> {
        let mut rects = match self.mode {
            LayoutMode::Dwindle => self.rects_dwindle(n, w, h),
            LayoutMode::Scroll => self.rects_scroll(n, w, h),
        };
        if self.zoomed && n > 0 {
            let f = self.focused.min(n - 1);
            for (i, r) in rects.iter_mut().enumerate() {
                if i == f {
                    *r = Rect { x: 0, y: 0, w, h };
                } else {
                    // Parked far off-screen (past any layer-shell zone
                    // offset the caller adds), size untouched so the client
                    // isn't resized.
                    r.x = -(r.w as i32) - PARK_OFFSET;
                }
            }
        }
        rects
    }

    /// Scroll mode: keep `view_x` valid, and (when `reveal`) scroll the
    /// minimum amount needed to bring the focused column fully into view.
    /// `reveal = false` is for click-focus: the column you clicked is already
    /// where you clicked it, moving the strip under the cursor is the bug.
    /// No-op (and `view_x` reset) outside Scroll mode.
    pub fn sync_view(&mut self, n: usize, w: u32) {
        let reveal = !std::mem::take(&mut self.skip_reveal);
        if self.mode != LayoutMode::Scroll || n == 0 {
            self.view_x = 0;
            return;
        }
        let col_w = self.col_w(w);
        let vw = w as i32;
        if reveal && !self.zoomed {
            let left = self.focused.min(n - 1) as i32 * col_w;
            let right = left + col_w;
            if left < self.view_x {
                self.view_x = left;
            } else if right > self.view_x + vw {
                self.view_x = right - vw;
            }
        }
        self.view_x = self.view_x.clamp(0, (n as i32 * col_w - vw).max(0));
    }

    fn col_w(&self, w: u32) -> i32 {
        let ratio = self.split_ratio.clamp(MIN_RATIO, MAX_RATIO);
        (((w as f32) * ratio).round().max(1.0)) as i32
    }

    /// A window opened (`n` windows now): it takes focus. Fake fullscreen is
    /// the user's decision, so it stays on: the new window takes over the
    /// zoom (the others stay parked at their normal size) instead of the
    /// zoomed window being tiled in half to make room.
    pub fn window_opened(&mut self, n: usize) {
        self.focused = n.saturating_sub(1);
    }

    /// Fake-fullscreen the focused window, or put it back.
    pub fn toggle_zoom(&mut self) {
        self.zoomed = !self.zoomed;
    }

    /// Switch between `Dwindle` and `Scroll`. Deliberately just these two —
    /// a master/stack third mode is cheap to add the same way later if
    /// wanted, but doesn't solve "more than ~4 windows" any better than
    /// dwindle does (still bounded by the fixed viewport), so it's not part
    /// of this pass.
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            LayoutMode::Dwindle => LayoutMode::Scroll,
            LayoutMode::Scroll => LayoutMode::Dwindle,
        };
    }

    /// Horizontal scrolling strip: every window gets a fixed-width column
    /// (`split_ratio` of output width, same clamp/keybinds as dwindle's
    /// primary-pane ratio), positioned left-to-right by index and offset by
    /// `view_x` (see `sync_view`). Columns off to
    /// either side simply have rects outside `[0,w)` — `blit()` in
    /// server.rs already clips to the composite buffer bounds for every
    /// blit regardless of mode, so an off-screen rect is a correct, safe
    /// no-op, not a special case this function has to handle.
    fn rects_scroll(&self, n: usize, w: u32, h: u32) -> Vec<Rect> {
        if n == 0 {
            return Vec::new();
        }
        let col_w = self.col_w(w);
        (0..n)
            .map(|i| Rect {
                x: i as i32 * col_w - self.view_x,
                y: 0,
                w: col_w as u32,
                h,
            })
            .collect()
    }

    /// Compute a rect for each of `n` windows filling `w`×`h`. Always returns
    /// exactly `n` rects; windows past the 4th reuse the last cell.
    fn rects_dwindle(&self, n: usize, w: u32, h: u32) -> Vec<Rect> {
        if n == 0 {
            return Vec::new();
        }
        let full = Rect { x: 0, y: 0, w, h };

        // Primary split (the outermost one) honors split_ratio; the
        // secondary split — subdividing whichever pane ISN'T primary —
        // always stays fixed 50/50. Both ratio and half variants computed
        // up front; each arm below picks whichever pair is "primary" for
        // its axis (columns when not flipped, rows when flipped).
        let ratio = self.split_ratio.clamp(MIN_RATIO, MAX_RATIO);
        let pw = ((w as f32) * ratio).round() as u32;
        let sw = w - pw;
        let ph = ((h as f32) * ratio).round() as u32;
        let sh = h - ph;
        let hw = w / 2;
        let hw2 = w - hw;
        let hh = h / 2;
        let hh2 = h - hh;

        let mut base: Vec<Rect> = match n {
            1 => vec![full],
            2 if self.flip => vec![
                Rect {
                    x: 0,
                    y: 0,
                    w,
                    h: ph,
                },
                Rect {
                    x: 0,
                    y: ph as i32,
                    w,
                    h: sh,
                },
            ],
            2 => vec![
                Rect {
                    x: 0,
                    y: 0,
                    w: pw,
                    h,
                },
                Rect {
                    x: pw as i32,
                    y: 0,
                    w: sw,
                    h,
                },
            ],
            3 if self.flip => vec![
                Rect {
                    x: 0,
                    y: 0,
                    w,
                    h: ph,
                },
                Rect {
                    x: 0,
                    y: ph as i32,
                    w: hw,
                    h: sh,
                },
                Rect {
                    x: hw as i32,
                    y: ph as i32,
                    w: hw2,
                    h: sh,
                },
            ],
            3 => vec![
                Rect {
                    x: 0,
                    y: 0,
                    w: pw,
                    h,
                },
                Rect {
                    x: pw as i32,
                    y: 0,
                    w: sw,
                    h: hh,
                },
                Rect {
                    x: pw as i32,
                    y: hh as i32,
                    w: sw,
                    h: hh2,
                },
            ],
            // 4+ → 2×2 grid; extras stack on the bottom-right cell below.
            // Grid is NOT split_ratio-adjustable — always fixed 50/50, same
            // as before resize existed.
            _ => vec![
                Rect {
                    x: 0,
                    y: 0,
                    w: hw,
                    h: hh,
                },
                Rect {
                    x: hw as i32,
                    y: 0,
                    w: hw2,
                    h: hh,
                },
                Rect {
                    x: 0,
                    y: hh as i32,
                    w: hw,
                    h: hh2,
                },
                Rect {
                    x: hw as i32,
                    y: hh as i32,
                    w: hw2,
                    h: hh2,
                },
            ],
        };

        // Pad so every window gets a rect (extras reuse the last cell).
        let last = *base.last().unwrap();
        while base.len() < n {
            base.push(last);
        }
        base
    }

    /// Grow the primary pane's share of the outermost split.
    pub fn resize_grow(&mut self) {
        self.split_ratio = (self.split_ratio + RESIZE_STEP).min(MAX_RATIO);
    }

    /// Shrink the primary pane's share of the outermost split.
    pub fn resize_shrink(&mut self) {
        self.split_ratio = (self.split_ratio - RESIZE_STEP).max(MIN_RATIO);
    }

    /// Move focus to the nearest window in `dir`, using rect centers. No-op if
    /// there's no window that way.
    pub fn focus(&mut self, rects: &[Rect], dir: Dir) {
        if rects.is_empty() {
            return;
        }
        if self.zoomed {
            // Everything but the focused window is parked off-screen, so
            // geometric search is meaningless: step through windows instead.
            let cur = self.focused.min(rects.len() - 1);
            self.focused = match dir {
                Dir::Left | Dir::Up => cur.saturating_sub(1),
                Dir::Right | Dir::Down => (cur + 1).min(rects.len() - 1),
            };
            return;
        }
        let cur = rects[self.focused.min(rects.len() - 1)];
        let (cx, cy) = cur.center();
        let mut best: Option<(usize, i64)> = None;
        for (i, r) in rects.iter().enumerate() {
            if i == self.focused {
                continue;
            }
            let (rx, ry) = r.center();
            let (dx, dy) = (rx - cx, ry - cy);
            // Require the candidate to lie predominantly in the asked direction.
            let ok = match dir {
                Dir::Left => dx < 0 && dx.abs() >= dy.abs(),
                Dir::Right => dx > 0 && dx.abs() >= dy.abs(),
                Dir::Up => dy < 0 && dy.abs() >= dx.abs(),
                Dir::Down => dy > 0 && dy.abs() >= dx.abs(),
            };
            if !ok {
                continue;
            }
            let dist = (dx as i64) * (dx as i64) + (dy as i64) * (dy as i64);
            if best.is_none_or(|(_, bd)| dist < bd) {
                best = Some((i, dist));
            }
        }
        if let Some((i, _)) = best {
            self.focused = i;
        }
    }

    /// Indices to swap so the focused window trades places with the next one
    /// (wrapping). Focus follows the moved window. Returns `None` for <2 windows.
    pub fn swap_next(&mut self, n: usize) -> Option<(usize, usize)> {
        if n < 2 {
            return None;
        }
        let a = self.focused.min(n - 1);
        let b = (a + 1) % n;
        self.focused = b;
        Some((a, b))
    }

    /// Flip the primary split axis (left/right ↔ top/bottom).
    pub fn rotate_split(&mut self) {
        self.flip = !self.flip;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_window_is_fullscreen() {
        let l = Layout::default();
        assert_eq!(
            l.rects(1, 100, 80),
            vec![Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 80
            }]
        );
    }

    #[test]
    fn two_windows_split_left_right() {
        let l = Layout::default();
        let r = l.rects(2, 100, 80);
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 50,
                h: 80
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 50,
                y: 0,
                w: 50,
                h: 80
            }
        );
    }

    #[test]
    fn flip_splits_top_bottom() {
        let l = Layout {
            focused: 0,
            flip: true,
            ..Default::default()
        };
        let r = l.rects(2, 100, 80);
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 40
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 0,
                y: 40,
                w: 100,
                h: 40
            }
        );
    }

    #[test]
    fn three_windows_corner() {
        let l = Layout::default();
        let r = l.rects(3, 100, 80);
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 50,
                h: 80
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 50,
                y: 0,
                w: 50,
                h: 40
            }
        );
        assert_eq!(
            r[2],
            Rect {
                x: 50,
                y: 40,
                w: 50,
                h: 40
            }
        );
    }

    #[test]
    fn four_windows_grid() {
        let l = Layout::default();
        let r = l.rects(4, 100, 80);
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 50,
                h: 40
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 50,
                y: 0,
                w: 50,
                h: 40
            }
        );
        assert_eq!(
            r[2],
            Rect {
                x: 0,
                y: 40,
                w: 50,
                h: 40
            }
        );
        assert_eq!(
            r[3],
            Rect {
                x: 50,
                y: 40,
                w: 50,
                h: 40
            }
        );
    }

    #[test]
    fn odd_dimensions_tile_without_gaps() {
        // 101×81: halves must cover the full extent (no lost row/column).
        let l = Layout::default();
        let r = l.rects(2, 101, 81);
        assert_eq!(r[0].w + r[1].w, 101);
        assert_eq!(r[1].x, r[0].w as i32);
    }

    #[test]
    fn extras_reuse_last_cell() {
        let l = Layout::default();
        let r = l.rects(6, 100, 80);
        assert_eq!(r.len(), 6);
        assert_eq!(r[5], r[3]); // stacks on bottom-right
    }

    #[test]
    fn focus_moves_right_then_left() {
        let mut l = Layout::default();
        let r = l.rects(2, 100, 80); // [left, right]
        l.focus(&r, Dir::Right);
        assert_eq!(l.focused, 1);
        l.focus(&r, Dir::Left);
        assert_eq!(l.focused, 0);
    }

    #[test]
    fn focus_noop_when_nothing_that_way() {
        let mut l = Layout::default();
        let r = l.rects(2, 100, 80);
        l.focus(&r, Dir::Left); // already leftmost
        assert_eq!(l.focused, 0);
    }

    #[test]
    fn swap_next_wraps_and_follows_focus() {
        let mut l = Layout::default();
        assert_eq!(l.swap_next(3), Some((0, 1)));
        assert_eq!(l.focused, 1);
        l.focused = 2;
        assert_eq!(l.swap_next(3), Some((2, 0)));
        assert_eq!(l.focused, 0);
        assert_eq!(l.swap_next(1), None);
    }

    #[test]
    fn resize_grows_and_shrinks_primary_pane() {
        let mut l = Layout::default();
        l.resize_grow();
        let r = l.rects(2, 100, 80);
        assert!(r[0].w > 50); // primary pane bigger than default 50/50
        assert_eq!(r[0].w + r[1].w, 100); // still covers the full width

        let mut l = Layout::default();
        l.resize_shrink();
        let r = l.rects(2, 100, 80);
        assert!(r[0].w < 50);
        assert_eq!(r[0].w + r[1].w, 100);
    }

    #[test]
    fn resize_clamps_at_bounds() {
        let mut l = Layout::default();
        for _ in 0..50 {
            l.resize_grow();
        }
        assert!((l.split_ratio - MAX_RATIO).abs() < f32::EPSILON);

        let mut l = Layout::default();
        for _ in 0..50 {
            l.resize_shrink();
        }
        assert!((l.split_ratio - MIN_RATIO).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_does_not_affect_2x2_grid() {
        let mut l = Layout::default();
        l.resize_grow();
        let r = l.rects(4, 100, 80);
        assert_eq!(
            r[0],
            Rect {
                x: 0,
                y: 0,
                w: 50,
                h: 40
            }
        );
        assert_eq!(
            r[1],
            Rect {
                x: 50,
                y: 0,
                w: 50,
                h: 40
            }
        );
    }

    #[test]
    fn resize_affects_primary_split_only_in_three_window_layout() {
        let mut l = Layout::default();
        l.resize_grow();
        let r = l.rects(3, 100, 80);
        assert!(r[0].w > 50); // primary column grew
                              // secondary (stacked) column's internal top/bottom split stays 50/50
        assert_eq!(r[1].h, 40);
        assert_eq!(r[2].h, 40);
    }

    #[test]
    fn toggle_mode_round_trips() {
        let mut l = Layout::default();
        assert_eq!(l.mode, LayoutMode::Dwindle);
        l.toggle_mode();
        assert_eq!(l.mode, LayoutMode::Scroll);
        l.toggle_mode();
        assert_eq!(l.mode, LayoutMode::Dwindle);
    }

    #[test]
    fn inset_and_inflate_round_trip_and_clamp() {
        let r = Rect { x: 10, y: 20, w: 100, h: 50 };
        assert_eq!(r.inset(4), Rect { x: 14, y: 24, w: 92, h: 42 });
        assert_eq!(r.inset(4).inflate(4), r);
        assert_eq!(Rect { x: 0, y: 0, w: 3, h: 3 }.inset(4).w, 1);
    }

    fn scroll() -> Layout {
        Layout {
            mode: LayoutMode::Scroll,
            ..Default::default()
        }
    }

    #[test]
    fn scroll_mode_starts_at_origin() {
        let mut l = scroll();
        l.sync_view(5, 100);
        let r = l.rects(5, 100, 80);
        assert_eq!(r[0], Rect { x: 0, y: 0, w: 50, h: 80 });
        assert_eq!(r[1].x, 50);
    }

    #[test]
    fn scroll_mode_scrolls_minimally_to_reveal_focus() {
        let mut l = scroll();
        l.focused = 2; // col 100..150, viewport 0..100 -> scroll by 50
        l.sync_view(5, 100);
        let r = l.rects(5, 100, 80);
        assert_eq!(r[2].x, 50);
        assert_eq!(r[1].x, 0); // previous column still visible
        l.focused = 1; // already fully visible: no movement
        l.sync_view(5, 100);
        assert_eq!(l.rects(5, 100, 80)[2].x, 50);
    }

    #[test]
    fn click_focus_does_not_scroll() {
        let mut l = scroll();
        l.focused = 2;
        l.sync_view(5, 100);
        let before = l.view_x;
        l.skip_reveal = true;
        l.focused = 0; // click on a partly-visible left column: strip stays put
        l.sync_view(5, 100);
        assert_eq!(l.view_x, before);
        assert!(!l.skip_reveal, "flag is consumed");
    }

    #[test]
    fn zoom_fills_focused_and_parks_the_rest() {
        let mut l = Layout { focused: 1, ..Default::default() };
        l.toggle_zoom();
        let r = l.rects(3, 100, 80);
        assert_eq!(r[1], Rect { x: 0, y: 0, w: 100, h: 80 });
        for i in [0, 2] {
            assert!(!r[i].contains(0, 0) && r[i].x + (r[i].w as i32) < -1000);
        }
        l.focus(&r, Dir::Right);
        assert_eq!(l.focused, 2);
        l.toggle_zoom();
        assert_ne!(l.rects(3, 100, 80)[1].w, 100);
    }

    #[test]
    fn a_new_window_does_not_unzoom_and_halve_the_old_one() {
        for mode in [LayoutMode::Dwindle, LayoutMode::Scroll] {
            let mut l = Layout { focused: 0, mode, ..Default::default() };
            l.toggle_zoom();
            // a second window opens on a zoomed first one
            l.window_opened(2);
            assert!(l.zoomed, "{mode:?}: fake fullscreen is the user's choice");
            assert_eq!(l.focused, 1, "{mode:?}: the new window takes focus");
            let r = l.rects(2, 100, 80);
            let full = Rect { x: 0, y: 0, w: 100, h: 80 };
            assert_eq!(r[1], full, "{mode:?}: the new window fills the area");
            // the old one is parked off-screen at its normal size, never halved
            assert!(r[0].x + (r[0].w as i32) < -1000, "{mode:?}");
            assert_ne!(r[0].w, 100, "{mode:?}: parked at its tiled size");
            // closing the new one by moving focus back restores the old one full-size
            l.focused = 0;
            assert_eq!(l.rects(1, 100, 80)[0], full, "{mode:?}");
        }
    }

    #[test]
    fn a_new_window_on_an_unzoomed_layout_just_takes_focus() {
        let mut l = Layout::default();
        l.window_opened(1);
        assert!(!l.zoomed);
        assert_eq!(l.focused, 0);
        l.window_opened(3);
        assert_eq!((l.focused, l.zoomed), (2, false));
        l.window_opened(0);
        assert_eq!(l.focused, 0, "no underflow");
    }

    #[test]
    fn scroll_mode_has_no_window_count_cap() {
        // The actual bug being fixed: dwindle stacks everything past 4 on
        // one cell. Scroll must give every window its own distinct rect
        // regardless of count.
        let l = Layout {
            mode: LayoutMode::Scroll,
            ..Default::default()
        };
        let r = l.rects(12, 100, 80);
        assert_eq!(r.len(), 12);
        let mut xs: Vec<i32> = r.iter().map(|rect| rect.x).collect();
        xs.sort();
        xs.dedup();
        assert_eq!(xs.len(), 12); // every column is at a distinct x — none reused
    }

    #[test]
    fn scroll_mode_resize_changes_column_width() {
        let mut l = Layout {
            mode: LayoutMode::Scroll,
            ..Default::default()
        };
        l.resize_grow();
        let r = l.rects(3, 100, 80);
        assert!(r[0].w > 50); // same split_ratio knob, reused as column width
    }

    #[test]
    fn scroll_mode_focus_navigates_and_autoscrolls() {
        let mut l = scroll();
        l.sync_view(5, 100);
        let r = l.rects(5, 100, 80);
        l.focus(&r, Dir::Right);
        assert_eq!(l.focused, 1);
        l.sync_view(5, 100);
        let r = l.rects(5, 100, 80);
        assert_eq!(r[1].x, 50);
        l.focus(&r, Dir::Right);
        l.sync_view(5, 100);
        assert_eq!(l.rects(5, 100, 80)[2].x, 50); // scrolled just enough
    }
}
