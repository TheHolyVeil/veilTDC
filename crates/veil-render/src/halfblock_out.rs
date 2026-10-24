//! Halfblock frame emitter with cell diffing and SGR elision (R5).
//!
//! The old emitter wrote a full `38;2;r;g;b;48;2;r;g;b` pair for every cell on
//! every frame (~40 B/cell). This one remembers the last frame and writes only
//! the cells that changed, with a cursor move per run and an SGR only when the
//! colours differ from the previous written cell. An unchanged frame emits
//! nothing at all.

use crate::palette::{bayer4, rgb_to_ansi16, rgb_to_ansi256};
use crate::ColorCell;
use std::fmt::Write as _;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorDepth {
    /// 24-bit `38;2`/`48;2`.
    True,
    /// xterm 256-colour palette `38;5`/`48;5`.
    Ansi256,
    /// 16-colour palette (fg 16, bg 8: the Linux console limit), dithered.
    Ansi16,
}

/// One cell reduced to what is actually sent: fg+bg RGB, or fg+bg palette ids.
type Key = [u8; 6];

#[derive(Default)]
pub struct HalfblockEmitter {
    prev: Vec<Key>,
    keys: Vec<Key>,
    prev_dims: (u16, u16),
    prev_depth: Option<ColorDepth>,
}

impl HalfblockEmitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the previous frame: the next `emit` redraws everything. Call
    /// after a resize, a mode change, or anything that may have scribbled on
    /// the terminal.
    pub fn invalidate(&mut self) {
        self.prev.clear();
        self.prev_depth = None;
    }

    fn key(c: &ColorCell, depth: ColorDepth, col: usize, row: usize) -> Key {
        match depth {
            ColorDepth::True => [c.fg[0], c.fg[1], c.fg[2], c.bg[0], c.bg[1], c.bg[2]],
            ColorDepth::Ansi256 => [rgb_to_ansi256(c.fg), rgb_to_ansi256(c.bg), 0, 0, 0, 0],
            ColorDepth::Ansi16 => {
                // Top pixel is sub-row 2*row, bottom 2*row+1 of the dither matrix.
                let bt = bayer4(col, row * 2) * 5;
                let bb = bayer4(col, row * 2 + 1) * 5;
                [rgb_to_ansi16(c.fg, 16, bt), rgb_to_ansi16(c.bg, 8, bb), 0, 0, 0, 0]
            }
        }
    }

    fn sgr(out: &mut String, k: &Key, depth: ColorDepth) {
        let _ = match depth {
            ColorDepth::True => write!(
                out,
                "\x1b[38;2;{};{};{};48;2;{};{};{}m",
                k[0], k[1], k[2], k[3], k[4], k[5]
            ),
            ColorDepth::Ansi256 => write!(out, "\x1b[38;5;{};48;5;{}m", k[0], k[1]),
            ColorDepth::Ansi16 => {
                let fg = if k[0] < 8 { 30 + k[0] } else { 90 + k[0] - 8 };
                // Leading 0 resets intensity: the Linux console turns 90-97
                // into "bold + 30-37" and the bold would otherwise stick to
                // every later cell.
                write!(out, "\x1b[0;{};{}m", fg, 40 + k[1])
            }
        };
    }

    /// Append the escape sequences that bring the screen from the previous
    /// frame to `cells`. Returns `false` (and appends nothing) when nothing
    /// changed. `cells` is `cols * rows` in row-major order.
    pub fn emit(
        &mut self,
        out: &mut String,
        cells: &[ColorCell],
        cols: u16,
        rows: u16,
        depth: ColorDepth,
    ) -> bool {
        let (c, r) = (cols as usize, rows as usize);
        if c == 0 || r == 0 || cells.len() < c * r {
            return false;
        }

        self.keys.clear();
        for row in 0..r {
            for col in 0..c {
                self.keys.push(Self::key(&cells[row * c + col], depth, col, row));
            }
        }

        let full = self.prev.len() != self.keys.len()
            || self.prev_dims != (cols, rows)
            || self.prev_depth != Some(depth);

        let start = out.len();
        let mut cur_sgr: Option<Key> = None;
        // Where the cursor sits after the last glyph we wrote.
        let mut cursor: Option<(usize, usize)> = None;
        for row in 0..r {
            for col in 0..c {
                let i = row * c + col;
                if !full && self.prev[i] == self.keys[i] {
                    continue;
                }
                if cursor != Some((row, col)) {
                    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
                }
                if cur_sgr != Some(self.keys[i]) {
                    Self::sgr(out, &self.keys[i], depth);
                    cur_sgr = Some(self.keys[i]);
                }
                out.push('▀');
                cursor = Some((row, col + 1));
            }
        }

        let wrote = out.len() > start;
        if wrote {
            out.push_str("\x1b[0m");
        }
        std::mem::swap(&mut self.prev, &mut self.keys);
        self.prev_dims = (cols, rows);
        self.prev_depth = Some(depth);
        wrote
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cols: u16, rows: u16, rgb: [u8; 3]) -> Vec<ColorCell> {
        vec![ColorCell { fg: rgb, bg: rgb }; cols as usize * rows as usize]
    }

    #[test]
    fn first_frame_is_full_then_identical_frame_is_silent() {
        let mut e = HalfblockEmitter::new();
        let g = grid(4, 3, [10, 20, 30]);
        let mut out = String::new();
        assert!(e.emit(&mut out, &g, 4, 3, ColorDepth::True));
        assert_eq!(out.matches('▀').count(), 12);
        out.clear();
        assert!(!e.emit(&mut out, &g, 4, 3, ColorDepth::True));
        assert!(out.is_empty());
    }

    #[test]
    fn one_changed_cell_emits_one_glyph() {
        let mut e = HalfblockEmitter::new();
        let mut g = grid(8, 4, [0, 0, 0]);
        let mut out = String::new();
        e.emit(&mut out, &g, 8, 4, ColorDepth::True);
        g[2 * 8 + 5].fg = [255, 0, 0];
        out.clear();
        assert!(e.emit(&mut out, &g, 8, 4, ColorDepth::True));
        assert_eq!(out.matches('▀').count(), 1);
        assert!(out.starts_with("\x1b[3;6H"));
        assert!(out.len() < 60, "diff should be tiny, got {} B", out.len());
    }

    #[test]
    fn same_colour_run_shares_one_sgr() {
        let mut e = HalfblockEmitter::new();
        let g = grid(10, 1, [9, 9, 9]);
        let mut out = String::new();
        e.emit(&mut out, &g, 10, 1, ColorDepth::True);
        assert_eq!(out.matches("38;2;").count(), 1);
        assert_eq!(out.matches("\x1b[1;").count(), 1); // single cursor move
    }

    #[test]
    fn invalidate_depth_and_resize_force_full_redraw() {
        let mut e = HalfblockEmitter::new();
        let g = grid(4, 2, [1, 2, 3]);
        let mut out = String::new();
        e.emit(&mut out, &g, 4, 2, ColorDepth::True);
        e.invalidate();
        out.clear();
        assert!(e.emit(&mut out, &g, 4, 2, ColorDepth::True));
        out.clear();
        assert!(e.emit(&mut out, &g, 4, 2, ColorDepth::Ansi256)); // depth changed
        out.clear();
        let g2 = grid(5, 2, [1, 2, 3]);
        assert!(e.emit(&mut out, &g2, 5, 2, ColorDepth::Ansi256)); // dims changed
        assert_eq!(out.matches('▀').count(), 10);
    }

    #[test]
    fn palette_depths_use_palette_sgr() {
        let g = grid(2, 1, [200, 30, 30]);
        let mut out = String::new();
        HalfblockEmitter::new().emit(&mut out, &g, 2, 1, ColorDepth::Ansi256);
        assert!(out.contains("\x1b[38;5;") && !out.contains("38;2;"));
        out.clear();
        HalfblockEmitter::new().emit(&mut out, &g, 2, 1, ColorDepth::Ansi16);
        assert!(out.contains("\x1b[0;"));
    }

    #[test]
    fn degenerate_grids_emit_nothing() {
        let mut e = HalfblockEmitter::new();
        let mut out = String::new();
        assert!(!e.emit(&mut out, &[], 0, 0, ColorDepth::True));
        assert!(!e.emit(&mut out, &[], 4, 0, ColorDepth::True));
        assert!(out.is_empty());
    }
}
