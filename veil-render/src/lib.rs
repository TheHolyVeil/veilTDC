use flate2::{write::ZlibEncoder, Compression};
use image::imageops;
use std::io::Write as _;

/// Zero-copy view over an already-owned RGBA buffer for feeding into
/// `image::imageops::resize`, which only needs `GenericImageView` — it
/// doesn't care whether the backing storage is owned or borrowed. Used by
/// `render_kitty_frame`'s downscale path so we don't have to clone the full
/// source frame just to hand ownership to an `ImageBuffer`.
type BorrowedRgba<'a> = image::ImageBuffer<image::Rgba<u8>, &'a [u8]>;

/* ── Half-block colour renderer ──────────────────────────────────────────── */

/// One terminal cell in half-block colour mode.
/// `▀` is always the character; fg = top pixel, bg = bottom pixel.
#[derive(Clone, PartialEq)]
pub struct ColorCell {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

fn sample_rgb(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
    let off = (y * width + x) as usize * 4;
    if off + 2 < rgba.len() { [rgba[off], rgba[off + 1], rgba[off + 2]] }
    else { [0, 0, 0] }
}

/// Convert an RGBA frame to half-block `ColorCell` grid.
///
/// Each terminal row maps to two source pixel rows via `▀` (top=fg, bot=bg),
/// doubling effective vertical resolution. Nearest-neighbour sampling.
pub fn rgba_to_halfblocks(rgba: &[u8], src_w: u32, src_h: u32, cols: u16, rows: u16) -> Vec<ColorCell> {
    let eff_h = rows as u32 * 2;
    let eff_w = cols as u32;
    let mut cells = Vec::with_capacity(cols as usize * rows as usize);
    for row in 0..rows as u32 {
        for col in 0..eff_w {
            let px_x  = col           * src_w / eff_w;
            let top_y = (row * 2)     * src_h / eff_h;
            let bot_y = (row * 2 + 1) * src_h / eff_h;
            cells.push(ColorCell {
                fg: sample_rgb(rgba, src_w, px_x, top_y),
                bg: sample_rgb(rgba, src_w, px_x, bot_y),
            });
        }
    }
    cells
}

const LUMA_MAP: &[char] = &[
    ' ', '.', '\'', '`', '^', '"', ',', ':', ';', 'I', 'l', '!', 'i',
    '>', '<', '~', '+', '_', '-', '?', ']', '[', '}', '{', '1', ')',
    '(', '|', '\\', '/', 't', 'f', 'j', 'r', 'x', 'n', 'u', 'v', 'c',
    'z', 'X', 'Y', 'U', 'J', 'C', 'L', 'Q', '0', 'O', 'Z', 'm', 'w',
    'q', 'p', 'd', 'b', 'k', 'h', 'a', 'o', '*', '#', 'M', 'W', '&',
    '8', '%', 'B', '@', '$',
];

pub fn luma_to_char(luma: u8) -> char {
    LUMA_MAP[(luma as usize * (LUMA_MAP.len() - 1)) / 255]
}

// ── TUI path ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Cell {
    pub ch: char,
    pub luma: u8,
}

impl Cell {
    pub fn rendered(&self) -> char {
        if self.ch.is_ascii_graphic() { self.ch } else { luma_to_char(self.luma) }
    }
}

pub struct TermFrame {
    pub cells: Vec<Cell>,
    pub width: u16,
    pub height: u16,
}

pub fn render_chars(frame: &TermFrame) -> Vec<char> {
    frame.cells.iter().map(|c| c.rendered()).collect()
}

// ── GUI path ──────────────────────────────────────────────────────────────────

/// Compute per-cell luma via box/area-average + Rec.601 weights, with
/// text-cell detection.
///
/// Samples every source texel in each cell's coverage rectangle — not a
/// single nearest-neighbour point. Point sampling aliases badly on
/// anti-aliased glyph edges: a single sample can land anywhere in a
/// sub-pixel, producing random luma values for cells that ought to look
/// similar. Box-averaging matches `luma.wgsl`'s GPU path exactly.
///
/// **Text-cell detection**: when the intra-cell pixel contrast (max−min
/// luma) exceeds [`TEXT_CONTRAST_T`], the cell likely contains an ink
/// glyph on a background. The ink-side luma is returned instead of the
/// box average. The average smears anti-aliased glyphs to mid-gray
/// (~128), which maps to mid-density `LUMA_MAP` characters regardless
/// of the glyph's shape; the ink minimum (dark-on-light) or maximum
/// (light-on-dark) pushes glyph cells to clearly dark or clearly bright
/// values, letting `luma_to_chars` produce density chars that reflect
/// actual ink presence. Adjacent glyph cells also become similarly
/// extreme, collapsing intra-region cell-to-cell contrast and reducing
/// false `|`/`-` edge detection inside text areas.
pub fn compute_luma(rgba: &[u8], src_w: u32, src_h: u32, cols: u16, rows: u16) -> Vec<u8> {
    /// Intra-cell contrast threshold for text-cell detection. 60/255 ≈ 23.5%
    /// — tuned so anti-aliased glyphs (typically 40–80% contrast) trigger it
    /// while smooth gradients and solid-colour regions don't.
    const TEXT_CONTRAST_T: u8 = 60;

    let cols_u = (cols as u32).max(1);
    let rows_u = (rows as u32).max(1);
    let mut out = Vec::with_capacity(cols as usize * rows as usize);

    for row in 0..rows_u {
        let y0 = row * src_h / rows_u;
        // Clamp to at least one row so upsampling (rows > src_h) degrades
        // to a point sample rather than an empty loop.
        let y1 = ((row + 1) * src_h / rows_u).max(y0 + 1).min(src_h);
        for col in 0..cols_u {
            let x0 = col * src_w / cols_u;
            let x1 = ((col + 1) * src_w / cols_u).max(x0 + 1).min(src_w);

            let mut sum:   u32 = 0;
            let mut min_l: u8  = 255;
            let mut max_l: u8  = 0;
            let mut count: u32 = 0;

            for py in y0..y1 {
                for px in x0..x1 {
                    let rgb = sample_rgb(rgba, src_w, px, py);
                    // Rec.601 weights (77+150+29 = 256); right-shift is a
                    // fast divide-by-256, identical to luma.wgsl's floats.
                    let l = ((rgb[0] as u32 * 77
                            + rgb[1] as u32 * 150
                            + rgb[2] as u32 * 29) >> 8) as u8;
                    sum   += l as u32;
                    if l < min_l { min_l = l; }
                    if l > max_l { max_l = l; }
                    count += 1;
                }
            }

            let avg      = (sum / count) as u8;
            let contrast = max_l.saturating_sub(min_l);

            // Text-cell: return the ink-side luma.
            //   Light background (avg > 128) → ink is the dark minimum.
            //   Dark  background (avg ≤ 128) → ink is the bright maximum.
            let luma = if contrast >= TEXT_CONTRAST_T {
                if avg > 128 { min_l } else { max_l }
            } else {
                avg
            };

            out.push(luma);
        }
    }
    out
}

pub fn apply_hysteresis(stable: &mut [u8], current: &[u8], threshold: u8) -> bool {
    let mut changed = false;
    for (s, &c) in stable.iter_mut().zip(current.iter()) {
        if (*s as i16 - c as i16).abs() >= threshold as i16 {
            *s = c;
            changed = true;
        }
    }
    changed
}

/// Map stabilised luma values to characters, with cell-level edge detection.
///
/// Each cell is compared against its left/right/above/below neighbours.
/// A sharp luma jump between neighbours means there's a UI boundary running
/// through this cell — render it as `|`, `-`, or `+` instead of a luma char.
/// This makes buttons, panels, and window chrome visually recognisable.
pub fn luma_to_chars(luma: &[u8], cols: u16, rows: u16) -> Vec<char> {
    let c = cols as usize;
    let r = rows as usize;

    let get = |row: i32, col: i32| -> u8 {
        if row < 0 || col < 0 || row >= r as i32 || col >= c as i32 {
            return 128; // neutral border value
        }
        luma[(row as usize * c) + col as usize]
    };

    // A cell is a UI edge when the contrast across it (neighbour-to-neighbour)
    // exceeds this threshold. Tuned for typical UI chrome contrast (>30) while
    // ignoring smooth gradients and hysteresis-stabilised noise.
    const EDGE_T: u8 = 38;

    let mut out = Vec::with_capacity(c * r);
    for row in 0..r as i32 {
        for col in 0..c as i32 {
            let l = get(row, col);

            // Horizontal contrast (left→right) → indicates a vertical edge `|`
            let horiz = (get(row, col - 1) as i16 - get(row, col + 1) as i16).unsigned_abs() as u8;
            // Vertical contrast (above→below) → indicates a horizontal edge `-`
            let vert  = (get(row - 1, col) as i16 - get(row + 1, col) as i16).unsigned_abs() as u8;

            let ch = if horiz > EDGE_T && vert > EDGE_T {
                '+'
            } else if horiz > EDGE_T {
                '|'
            } else if vert > EDGE_T {
                '-'
            } else {
                luma_to_char(l)
            };

            out.push(ch);
        }
    }
    out
}

// ── Text overlay ──────────────────────────────────────────────────────────────

/// A text element placed at a specific terminal cell position.
/// Produced by the AT-SPI query and stamped over the luma/edge render.
#[derive(Clone)]
pub struct TextCell {
    pub col:  u16,
    pub row:  u16,
    pub text: String,
}

/// Stamp AT-SPI text elements over an already-rendered char grid.
/// Text is truncated at the right edge of the terminal.
pub fn apply_text_overlay(chars: &mut [char], text: &[TextCell], cols: u16) {
    for tc in text {
        let base = tc.row as usize * cols as usize + tc.col as usize;
        let space = cols as usize - tc.col as usize;
        for (i, ch) in tc.text.chars().take(space).enumerate() {
            if let Some(slot) = chars.get_mut(base + i) {
                // Only stamp printable ASCII — skip control chars and wide unicode
                if ch.is_ascii_graphic() || ch == ' ' {
                    *slot = ch;
                }
            }
        }
    }
}

// ── Kitty graphics protocol ───────────────────────────────────────────────────

/// Encode an RGBA frame as a Kitty graphics protocol escape sequence.
///
/// Downsamples to `cols × rows*2` pixels (one kitty "pixel row" per half-block
/// row), base64-encodes the raw RGBA, and emits chunked APC sequences.
/// Returns a single string: delete-previous + full new frame.
///
/// Protocol: `ESC_G<params>;<b64>ESC\`  (APC, not OSC)
/// Delete the persistent image ID used by render_kitty_frame.
/// Call on resize and exit — not between frames.
pub const KITTY_DELETE: &str = "\x1b_Ga=d,d=i,i=1,q=2\x1b\\";

pub fn render_kitty_frame(rgba: &[u8], src_w: u32, src_h: u32, cols: u16, rows: u16) -> String {
    const CHUNK: usize = 4096;
    // Cap pixel dimensions so the base64 payload stays small enough to
    // transmit without mid-sequence flicker through the PTY buffer.
    const MAX_W: u32 = 960;
    const MAX_H: u32 = 540;

    if rgba.is_empty() || src_w == 0 || src_h == 0 { return String::new(); }

    // `iw`/`ih`/`buf` used to always clone the full source frame via
    // `rgba.to_vec()` even in the common case (no downscale needed) purely
    // to unify the type before compression — every frame, whether or not
    // any resize work was actually happening. `buf` is now a `Cow`: the
    // common path borrows `rgba` directly (zero-copy) and only the
    // downscale path produces an owned buffer (unavoidable — resizing
    // writes new pixels). The downscale path itself is also zero-copy on
    // input now: `BorrowedRgba` reads straight out of `rgba` instead of
    // `.to_vec()`-ing it first just to feed the resizer.
    let (iw, ih, buf): (u32, u32, std::borrow::Cow<[u8]>) = if src_w > MAX_W || src_h > MAX_H {
        let img: BorrowedRgba = match image::ImageBuffer::from_raw(src_w, src_h, rgba) {
            Some(i) => i,
            None    => return String::new(),
        };
        let scaled = imageops::resize(&img, MAX_W, MAX_H, imageops::FilterType::Triangle);
        let (w, h) = scaled.dimensions();
        (w, h, std::borrow::Cow::Owned(scaled.into_raw()))
    } else {
        (src_w, src_h, std::borrow::Cow::Borrowed(rgba))
    };

    // Zlib-compress before base64 — typical UI content compresses 4-6x,
    // bringing 1.5MB frames down to ~300KB and making real-time feasible.
    let compressed = {
        let mut enc = ZlibEncoder::new(Vec::with_capacity(buf.len() / 4), Compression::fast());
        let _ = enc.write_all(&buf);
        enc.finish().unwrap_or_else(|_| buf.into_owned())
    };

    let b64 = base64_encode(&compressed);
    let b64_bytes = b64.as_bytes();
    let num_chunks = (b64_bytes.len() + CHUNK - 1).max(1) / CHUNK;
    let mut out = String::with_capacity(b64.len() + num_chunks * 80);

    for (i, chunk) in b64_bytes.chunks(CHUNK).enumerate() {
        let s    = std::str::from_utf8(chunk).unwrap_or("");
        let more = if i + 1 < num_chunks { 1 } else { 0 };
        if i == 0 {
            use std::fmt::Write as _;
            // o=z: zlib payload  i=1,p=1: stable IDs for atomic in-place update
            let _ = write!(
                out,
                "\x1b_Ga=T,f=32,o=z,i=1,p=1,s={iw},v={ih},c={cols},r={rows},q=2,m={more};{s}\x1b\\"
            );
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "\x1b_Gm={more};{s}\x1b\\");
        }
    }

    out
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b0 = c[0] as u32;
        let b1 = c.get(1).copied().unwrap_or(0) as u32;
        let b2 = c.get(2).copied().unwrap_or(0) as u32;
        let n  = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize]);
        out.push(T[((n >> 12) & 63) as usize]);
        out.push(if c.len() > 1 { T[((n >> 6) & 63) as usize] } else { b'=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] } else { b'=' });
    }
    String::from_utf8(out).unwrap()
}
