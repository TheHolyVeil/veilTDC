//! Preview the sub-cell renderers on an image.
//!
//! cargo run -p veil-render --example braille_preview -- <image> \
//!     [--cols N] [--rows N] [--halfblock | --quadrant] [--midrange] [--dump] [--png out.png]
//!
//! Default mode is braille, printed to stdout as truecolor ANSI.
//! `--dump`  prints the plain character grid (no colour) — diffable / greppable.
//! `--png`   rasterises the cell grid back to pixels (8×16 per cell) so you can
//!           inspect the glyph geometry without a terminal. This is NOT a real
//!           font render — see `render_real_font.py` for that.

use image::{Rgb, RgbImage};
use std::fmt::Write as _;
use veil_render::{
    rgba_to_braille_with, rgba_to_halfblocks, rgba_to_quadrant_with, BrailleCell, Threshold, QUAD_CP,
};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Braille,
    Halfblock,
    Quadrant,
}

/// Braille dot bit per row-major 2×4 sub-pixel (mirrors the lib's private table).
const BRAILLE_BIT: [u32; 8] = [0x01, 0x08, 0x02, 0x10, 0x04, 0x20, 0x40, 0x80];

/// Is sub-pixel `i` (row-major) of a cell drawn in `ch` painted with the fg colour?
fn sub_on(mode: Mode, ch: char, i: usize) -> bool {
    match mode {
        Mode::Halfblock => i == 0,
        Mode::Quadrant => QUAD_CP
            .iter()
            .position(|&q| q == ch)
            .is_some_and(|m| m >> i & 1 == 1),
        Mode::Braille => {
            let b = (ch as u32).wrapping_sub(0x2800);
            b < 256 && b & BRAILLE_BIT[i] != 0
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut path, mut png, mut mode, mut dump) = (None, None, Mode::Braille, false);
    let mut thr = Threshold::Mean;
    let (mut cols, mut rows) = (210u16, 50u16);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dump" => dump = true,
            "--midrange" => thr = Threshold::MidRange,
            "--braille" => mode = Mode::Braille,
            "--halfblock" => mode = Mode::Halfblock,
            "--quadrant" => mode = Mode::Quadrant,
            "--png" => png = args.next(),
            "--cols" => cols = args.next().and_then(|v| v.parse().ok()).unwrap_or(cols),
            "--rows" => rows = args.next().and_then(|v| v.parse().ok()).unwrap_or(rows),
            flag if flag.starts_with("--") => {
                eprintln!("unknown flag {flag}");
                std::process::exit(2);
            }
            _ => path = Some(a),
        }
    }
    let (cols, rows) = (cols.max(1), rows.max(1));
    let Some(path) = path else {
        eprintln!("usage: braille_preview <image> [--cols N] [--rows N] [--halfblock|--quadrant] [--midrange] [--dump] [--png out.png]");
        std::process::exit(2);
    };
    let img = image::open(&path)
        .unwrap_or_else(|e| {
            eprintln!("can't open {path}: {e}");
            std::process::exit(1);
        })
        .to_rgba8();
    let (w, h) = img.dimensions();
    let raw = img.as_raw();

    let cells: Vec<BrailleCell> = match mode {
        Mode::Braille => rgba_to_braille_with(raw, w, h, cols, rows, thr),
        Mode::Quadrant => rgba_to_quadrant_with(raw, w, h, cols, rows, thr),
        Mode::Halfblock => rgba_to_halfblocks(raw, w, h, cols, rows)
            .into_iter()
            .map(|c| BrailleCell { ch: '▀', fg: c.fg, bg: c.bg })
            .collect(),
    };

    let mut out = String::new();
    for row in cells.chunks(cols as usize) {
        for c in row {
            if dump {
                out.push(c.ch);
            } else {
                let _ = write!(
                    out,
                    "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m{}",
                    c.fg[0], c.fg[1], c.fg[2], c.bg[0], c.bg[1], c.bg[2], c.ch
                );
            }
        }
        out.push_str(if dump { "\n" } else { "\x1b[0m\n" });
    }
    print!("{out}");

    if let Some(png) = png {
        const CW: u32 = 8;
        const CH: u32 = 16;
        let (sw, sh) = match mode {
            Mode::Braille => (2, 4),
            Mode::Quadrant => (2, 2),
            Mode::Halfblock => (1, 2),
        };
        let mut im = RgbImage::new(cols as u32 * CW, rows as u32 * CH);
        for (n, c) in cells.iter().enumerate() {
            let (cx, cy) = ((n % cols as usize) as u32, (n / cols as usize) as u32);
            for py in 0..CH {
                for px in 0..CW {
                    let i = ((py * sh / CH) * sw + px * sw / CW) as usize;
                    let rgb = if sub_on(mode, c.ch, i) { c.fg } else { c.bg };
                    im.put_pixel(cx * CW + px, cy * CH + py, Rgb(rgb));
                }
            }
        }
        im.save(&png).unwrap_or_else(|e| eprintln!("can't write {png}: {e}"));
        eprintln!("wrote {png} ({}×{})", im.width(), im.height());
    }
}
