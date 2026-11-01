//! Geometry of the toast stack. Pure: it takes the measured toast heights and
//! the output size and returns rectangles, so the drawing code (step 5) only
//! paints what it is told and click handling can use [`Stack::hit`] unchanged.
//!
//! The surface is anchored top-right and sized to exactly the stack; its input
//! region is the toast rectangles only, so clicks anywhere else go to the
//! windows underneath.
//!
//! Known edge: on a very short output the layout may hide toasts that the
//! timers still count as visible; they can expire unseen. Normal screens never hit it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn contains(&self, px: i32, py: i32) -> bool {
        let (px, py) = (i64::from(px), i64::from(py));
        px >= i64::from(self.x)
            && px < i64::from(self.x) + i64::from(self.w)
            && py >= i64::from(self.y)
            && py < i64::from(self.y) + i64::from(self.h)
    }

    #[cfg(test)]
    fn bottom(&self) -> i64 {
        i64::from(self.y) + i64::from(self.h)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct LayoutConfig {
    pub width: u32,
    pub gap: u32,
    /// Distance from the top edge of the output. Set it to the bar height plus
    /// the desired gap, so toasts clear the bar.
    pub margin_top: u32,
    pub margin_right: u32,
    pub margin_bottom: u32,
    /// Height of the "+n more" line.
    pub overflow_height: u32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        LayoutConfig { width: 360, gap: 8, margin_top: 8, margin_right: 12, margin_bottom: 12, overflow_height: 28 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Toast(u32),
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stack {
    /// The layer surface in output coordinates, or `None` when there is nothing
    /// to show (the surface should be unmapped).
    pub surface: Option<Rect>,
    /// Newest first, top to bottom. Rectangles are surface-local.
    pub toasts: Vec<(u32, Rect)>,
    /// The "+n more" line, when there are hidden toasts and room for it.
    pub overflow: Option<Rect>,
    /// Toasts not shown, for the "+n more" text: those older than the visible
    /// set plus any the screen could not fit.
    pub hidden: usize,
}

impl Stack {
    /// Rectangles that should receive pointer input (surface-local).
    pub fn input_rects(&self) -> Vec<Rect> {
        self.toasts.iter().map(|&(_, r)| r).chain(self.overflow).collect()
    }

    /// What is under a surface-local point; `None` in the gaps.
    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        if let Some(&(id, _)) = self.toasts.iter().find(|(_, r)| r.contains(x, y)) {
            return Some(Hit::Toast(id));
        }
        self.overflow.filter(|r| r.contains(x, y)).map(|_| Hit::Overflow)
    }
}

/// Lay out `toasts` (id and measured height, newest first). `hidden_older` is
/// the count the timers hold back (older than the visible set).
pub fn stack(cfg: &LayoutConfig, out: OutputSize, toasts: &[(u32, u32)], hidden_older: usize) -> Stack {
    let avail = out.height.saturating_sub(cfg.margin_top).saturating_sub(cfg.margin_bottom);
    let width = cfg.width.min(out.width.saturating_sub(cfg.margin_right)).max(1);
    let n = toasts.len();

    let mut placed: Vec<(u32, Rect)> = Vec::new();
    let mut used: u32 = 0;
    for (i, &(id, h)) in toasts.iter().enumerate() {
        let h = h.clamp(1, avail.max(1));
        let gap = if placed.is_empty() { 0 } else { cfg.gap };
        let top = used.saturating_add(gap);
        let end = top.saturating_add(h);
        let more_after = n - i - 1 + hidden_older > 0;
        let reserve = if more_after { cfg.gap.saturating_add(cfg.overflow_height) } else { 0 };
        // The first toast is always shown, even on a tiny output.
        if !placed.is_empty() && end.saturating_add(reserve) > avail {
            break;
        }
        placed.push((id, Rect { x: 0, y: top as i32, w: width, h }));
        used = end;
    }

    let hidden = hidden_older + (n - placed.len());
    let gap = if placed.is_empty() { 0 } else { cfg.gap };
    let overflow_top = used.saturating_add(gap);
    let overflow_end = overflow_top.saturating_add(cfg.overflow_height);
    let overflow = (hidden > 0 && overflow_end <= avail.max(1))
        .then_some(Rect { x: 0, y: overflow_top as i32, w: width, h: cfg.overflow_height });

    let total = match overflow {
        Some(_) => overflow_end,
        None => used,
    };
    let surface = (total > 0).then(|| Rect {
        x: out.width as i32 - cfg.margin_right as i32 - width as i32,
        y: cfg.margin_top as i32,
        w: width,
        h: total,
    });
    Stack { surface, toasts: placed, overflow, hidden }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: OutputSize = OutputSize { width: 1920, height: 1080 };

    fn cfg() -> LayoutConfig {
        LayoutConfig::default()
    }

    #[test]
    fn nothing_to_show_means_no_surface() {
        let s = stack(&cfg(), OUT, &[], 0);
        assert_eq!(s.surface, None);
        assert!(s.toasts.is_empty() && s.overflow.is_none() && s.input_rects().is_empty());
    }

    #[test]
    fn one_toast_sits_top_right() {
        let s = stack(&cfg(), OUT, &[(7, 80)], 0);
        assert_eq!(s.surface, Some(Rect { x: 1920 - 12 - 360, y: 8, w: 360, h: 80 }));
        assert_eq!(s.toasts, vec![(7, Rect { x: 0, y: 0, w: 360, h: 80 })]);
        assert_eq!(s.hidden, 0);
    }

    #[test]
    fn toasts_stack_newest_first_with_gaps() {
        let s = stack(&cfg(), OUT, &[(3, 80), (2, 100), (1, 60)], 0);
        let ys: Vec<_> = s.toasts.iter().map(|(id, r)| (*id, r.y, r.h)).collect();
        assert_eq!(ys, vec![(3, 0, 80), (2, 88, 100), (1, 196, 60)]);
        assert_eq!(s.surface.unwrap().h, 256, "80 + 8 + 100 + 8 + 60");
    }

    #[test]
    fn surface_clears_the_bar() {
        let c = LayoutConfig { margin_top: 40 + 8, ..cfg() };
        let s = stack(&c, OUT, &[(1, 80)], 0);
        assert_eq!(s.surface.unwrap().y, 48, "bar height 40 plus an 8 px gap");
    }

    #[test]
    fn overflow_line_follows_the_last_toast() {
        let s = stack(&cfg(), OUT, &[(2, 80), (1, 80)], 3);
        assert_eq!(s.hidden, 3);
        assert_eq!(s.overflow, Some(Rect { x: 0, y: 176, w: 360, h: 28 }));
        assert_eq!(s.surface.unwrap().h, 204);
        assert_eq!(s.input_rects().len(), 3);
    }

    #[test]
    fn short_output_pushes_older_toasts_into_the_overflow() {
        let out = OutputSize { width: 1920, height: 300 };
        let toasts: Vec<(u32, u32)> = (1..=5).rev().map(|id| (id, 80)).collect();
        let s = stack(&cfg(), out, &toasts, 0);
        // 300 - 8 - 12 = 280 available; two toasts (168) plus the 36 px overflow line fit.
        assert_eq!(s.toasts.iter().map(|t| t.0).collect::<Vec<_>>(), vec![5, 4]);
        assert_eq!(s.hidden, 3);
        assert_eq!(s.overflow.unwrap().y, 176);
        assert_eq!(s.surface.unwrap().h, 204);
    }

    #[test]
    fn an_oversized_first_toast_is_clamped_not_dropped() {
        let out = OutputSize { width: 1920, height: 100 };
        let s = stack(&cfg(), out, &[(2, 70), (1, 70)], 0);
        assert_eq!(s.toasts.len(), 1);
        assert_eq!(s.toasts[0].1.h, 70);
        assert_eq!(s.hidden, 1);
        assert_eq!(s.overflow, None, "no room for the line, the count is still reported");
        let s = stack(&cfg(), out, &[(1, 500)], 0);
        assert_eq!(s.toasts[0].1.h, 80, "clamped to the available height");
    }

    #[test]
    fn narrow_output_shrinks_the_width() {
        let out = OutputSize { width: 300, height: 1080 };
        let s = stack(&cfg(), out, &[(1, 50)], 0);
        assert_eq!(s.surface.unwrap().w, 288);
        assert_eq!(s.surface.unwrap().x, 0);
    }

    #[test]
    fn clicks_in_gaps_and_outside_fall_through() {
        let s = stack(&cfg(), OUT, &[(2, 80), (1, 80)], 2);
        assert_eq!(s.hit(10, 10), Some(Hit::Toast(2)));
        assert_eq!(s.hit(359, 167), Some(Hit::Toast(1)));
        assert_eq!(s.hit(10, 84), None, "gap between toasts");
        assert_eq!(s.hit(10, 180), Some(Hit::Overflow));
        assert_eq!(s.hit(360, 10), None, "right of the surface");
        assert_eq!(s.hit(-1, 10), None);
        assert_eq!(s.hit(10, 400), None);
    }

    #[test]
    fn layouts_never_overlap_or_overflow_the_screen() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for _ in 0..3000 {
            let out = OutputSize { width: 400 + next(2000) as u32, height: 200 + next(1200) as u32 };
            let toasts: Vec<(u32, u32)> = (0..next(10) as u32).map(|i| (i + 1, 1 + next(250) as u32)).collect();
            let hidden_older = next(5) as usize;
            let s = stack(&cfg(), out, &toasts, hidden_older);
            let avail = i64::from(out.height) - 20;

            assert_eq!(s.hidden, hidden_older + toasts.len() - s.toasts.len());
            let rects = s.input_rects();
            let mut prev_bottom = 0i64;
            for r in &rects {
                assert!(i64::from(r.y) >= prev_bottom, "overlap in {s:?}");
                prev_bottom = r.bottom();
                assert_eq!(r.w, s.surface.unwrap().w);
            }
            if let Some(surface) = s.surface {
                assert!(rects.iter().all(|r| r.bottom() <= i64::from(surface.h)), "outside the surface in {s:?}");
                assert!(i64::from(surface.h) <= avail, "taller than the screen allows in {s:?}");
            } else {
                assert!(toasts.is_empty() && hidden_older == 0);
            }
        }
    }
}
