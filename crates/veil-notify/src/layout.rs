//! Vertical placement of the toast stack.
//!
//! Every toast is its own layer surface anchored top-right, so a toast's
//! position is just its top margin. The compositor measures that margin from
//! the usable area, which already excludes the bar (checked on the real host in
//! `crates/veil/tests/layer_shell.rs`), so `margin_top` is only the visual gap
//! and must not include the bar height. Gaps between toasts are real gaps:
//! nothing is there, so clicks fall through without any input-region tricks.

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
}

#[derive(Clone, Debug)]
pub struct StackConfig {
    /// Gap below the top of the usable area (not the bar height).
    pub margin_top: u32,
    pub margin_right: u32,
    /// Space between toasts.
    pub gap: u32,
    /// The stack never grows taller than this; older toasts are not shown.
    pub max_height: u32,
}

impl Default for StackConfig {
    fn default() -> Self {
        StackConfig { margin_top: 8, margin_right: 12, gap: 8, max_height: 900 }
    }
}

/// Top margins for the toasts that fit, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub margins: Vec<i32>,
}

impl Placement {
    /// How many toasts get a surface.
    pub fn shown(&self) -> usize {
        self.margins.len()
    }
}

/// Place toasts of the given `heights` (newest first, top to bottom). The first
/// toast is always shown; later ones only while the stack stays within
/// `max_height`.
pub fn place(cfg: &StackConfig, heights: &[u32]) -> Placement {
    let limit = cfg.margin_top.saturating_add(cfg.max_height);
    let mut margins = Vec::new();
    let mut y = cfg.margin_top;
    for (i, &h) in heights.iter().enumerate() {
        let bottom = y.saturating_add(h);
        if i > 0 && bottom > limit {
            break;
        }
        margins.push(i32::try_from(y).unwrap_or(i32::MAX));
        y = bottom.saturating_add(cfg.gap);
    }
    Placement { margins }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_place() {
        assert_eq!(place(&StackConfig::default(), &[]).shown(), 0);
    }

    #[test]
    fn toasts_stack_downwards_with_gaps() {
        let p = place(&StackConfig::default(), &[80, 100, 60]);
        assert_eq!(p.margins, vec![8, 8 + 80 + 8, 8 + 80 + 8 + 100 + 8]);
    }

    #[test]
    fn the_first_toast_is_always_shown_later_ones_need_room() {
        let cfg = StackConfig { max_height: 200, ..StackConfig::default() };
        assert_eq!(place(&cfg, &[500]).shown(), 1, "even when it alone is too tall");
        assert_eq!(place(&cfg, &[120, 120, 120]).shown(), 1);
        assert_eq!(place(&cfg, &[80, 80, 80]).shown(), 2, "80 + 8 + 80 fits, a third would not");
    }

    #[test]
    fn margin_top_is_only_the_visual_gap() {
        // The compositor already starts the usable area below the bar.
        assert_eq!(place(&StackConfig::default(), &[50]).margins, vec![8]);
    }

    #[test]
    fn huge_values_do_not_overflow() {
        let cfg = StackConfig { margin_top: u32::MAX - 1, max_height: u32::MAX, ..StackConfig::default() };
        let _ = place(&cfg, &[u32::MAX, u32::MAX, 5]);
    }

    #[test]
    fn rect_contains_is_half_open() {
        let r = Rect { x: 10, y: 10, w: 5, h: 5 };
        assert!(r.contains(10, 10) && r.contains(14, 14));
        assert!(!r.contains(15, 10) && !r.contains(9, 10) && !r.contains(10, 15));
    }
}
