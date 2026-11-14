//! Background blur for the software compositor (phase 6c).
//!
//! A surface that asked for `ext-background-effect` gets the pixels already
//! composited *beneath* it blurred in place, just before it is drawn. The
//! rectangle is downscaled, box-blurred at the small size, and scaled back up
//! with bilinear filtering, so the cost follows the blurred area, not the
//! radius. Everything here is pure pixel and rectangle maths (no Wayland), so
//! it is unit-tested without a compositor.

use crate::layout::Rect;

/// Reusable work buffers, so blurring allocates nothing in steady state.
#[derive(Default)]
pub struct BlurScratch {
    small: Vec<u8>,
    tmp: Vec<u8>,
}

/// Turn a region's add/subtract list into disjoint-enough add rectangles.
/// `true` = add. A subtract splits every rectangle it cuts into up to four
/// pieces. Overlapping adds are kept as they are (they blur twice where they
/// overlap, which clients do not normally ask for).
pub fn region_rects(ops: &[(bool, Rect)]) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    for &(add, r) in ops {
        if r.is_empty() {
            continue;
        }
        if add {
            out.push(r);
        } else {
            let mut next = Vec::with_capacity(out.len() + 3);
            for o in out.drain(..) {
                subtract_rect(o, r, &mut next);
            }
            out = next;
        }
    }
    out
}

/// `a` minus `b`, as up to four rectangles pushed onto `out`.
fn subtract_rect(a: Rect, b: Rect, out: &mut Vec<Rect>) {
    let i = a.intersect(&b);
    if i.is_empty() {
        out.push(a);
        return;
    }
    let (ay1, ax1) = (a.y + a.h as i32, a.x + a.w as i32);
    let (iy1, ix1) = (i.y + i.h as i32, i.x + i.w as i32);
    if i.y > a.y {
        out.push(Rect { x: a.x, y: a.y, w: a.w, h: (i.y - a.y) as u32 });
    }
    if iy1 < ay1 {
        out.push(Rect { x: a.x, y: iy1, w: a.w, h: (ay1 - iy1) as u32 });
    }
    if i.x > a.x {
        out.push(Rect { x: a.x, y: i.y, w: (i.x - a.x) as u32, h: i.h });
    }
    if ix1 < ax1 {
        out.push(Rect { x: ix1, y: i.y, w: (ax1 - ix1) as u32, h: i.h });
    }
}

/// Grow `damage` until it fully covers every footprint it touches.
///
/// A blur reads the pixels around the ones that changed. With damage-limited
/// compositing those neighbours may be last frame's *already blurred* pixels,
/// which would blur again and again. Repainting the whole footprint whenever
/// any part of it is damaged keeps every blur input fresh. Growing can make
/// it touch another footprint, so this runs to a fixed point.
pub fn expand_damage(mut damage: Rect, footprints: &[Rect]) -> Rect {
    if damage.is_empty() {
        return damage;
    }
    loop {
        let mut changed = false;
        for f in footprints {
            if !f.is_empty() && !damage.intersect(f).is_empty() {
                let u = damage.union(f);
                if u != damage {
                    damage = u;
                    changed = true;
                }
            }
        }
        if !changed {
            return damage;
        }
    }
}

/// Downscale factor for a radius: bigger blurs work on a smaller copy.
fn factor(radius: u32) -> usize {
    match radius {
        0..=5 => 1,
        6..=15 => 2,
        _ => 4,
    }
}

/// Blur `rect` of the RGBA buffer `back` (`w` x `h`) in place, writing only
/// inside `clip`. The blur samples only inside `rect` (edges clamp), so it
/// never reads pixels outside the footprint. `strength` mixes the blurred
/// result with what was there (1.0 = fully blurred, used to follow a fade).
/// Alpha is left alone.
#[allow(clippy::too_many_arguments)]
pub fn blur_rect(
    back: &mut [u8],
    w: u32,
    h: u32,
    rect: Rect,
    clip: Rect,
    radius: u32,
    strength: f32,
    scratch: &mut BlurScratch,
) {
    let r = rect.clamp_to(w, h);
    let dst = r.intersect(&clip);
    if r.is_empty() || dst.is_empty() || radius == 0 || strength <= 0.0 {
        return;
    }
    if back.len() < (w as usize) * (h as usize) * 4 {
        return;
    }
    let f = factor(radius);
    let sw = (r.w as usize).div_ceil(f);
    let sh = (r.h as usize).div_ceil(f);
    scratch.small.resize(sw * sh * 3, 0);
    scratch.tmp.resize(sw * sh * 3, 0);
    downsample(back, w as usize, r, f, sw, sh, &mut scratch.small);
    // Two H+V box passes approximate a Gaussian; the half-width is chosen so
    // the total reach is about `radius` output pixels.
    let rb = ((radius as usize + f) / (2 * f)).max(1);
    for _ in 0..2 {
        box_pass(&scratch.small, &mut scratch.tmp, sw, sh, rb, true);
        box_pass(&scratch.tmp, &mut scratch.small, sw, sh, rb, false);
    }
    upsample(back, w as usize, r, dst, f, sw, sh, &scratch.small, strength);
}

fn downsample(back: &[u8], stride_px: usize, r: Rect, f: usize, sw: usize, sh: usize, out: &mut [u8]) {
    let (rx, ry, rw, rh) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
    for sy in 0..sh {
        let (y0, y1) = (sy * f, (sy * f + f).min(rh));
        for sx in 0..sw {
            let (x0, x1) = (sx * f, (sx * f + f).min(rw));
            let (mut a, mut b, mut c) = (0u32, 0u32, 0u32);
            for y in y0..y1 {
                let row = ((ry + y) * stride_px + rx) * 4;
                for x in x0..x1 {
                    let i = row + x * 4;
                    a += back[i] as u32;
                    b += back[i + 1] as u32;
                    c += back[i + 2] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (sy * sw + sx) * 3;
            out[o] = ((a + n / 2) / n) as u8;
            out[o + 1] = ((b + n / 2) / n) as u8;
            out[o + 2] = ((c + n / 2) / n) as u8;
        }
    }
}

/// One running-sum box blur pass over 3-channel pixels, edges clamped.
fn box_pass(src: &[u8], dst: &mut [u8], sw: usize, sh: usize, r: usize, horizontal: bool) {
    let (lines, len, es, ls) = if horizontal { (sh, sw, 3, sw * 3) } else { (sw, sh, sw * 3, 3) };
    let win = (2 * r + 1) as u32;
    for line in 0..lines {
        for c in 0..3 {
            let base = line * ls + c;
            let at = |i: usize| src[base + i * es] as u32;
            let mut sum = 0u32;
            for k in 0..=2 * r {
                sum += at((k as isize - r as isize).clamp(0, len as isize - 1) as usize);
            }
            for x in 0..len {
                dst[base + x * es] = ((sum + win / 2) / win) as u8;
                sum = sum + at((x + r + 1).min(len - 1)) - at(x.saturating_sub(r));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn upsample(
    back: &mut [u8],
    stride_px: usize,
    r: Rect,
    dst: Rect,
    f: usize,
    sw: usize,
    sh: usize,
    small: &[u8],
    strength: f32,
) {
    let mix = (strength.clamp(0.0, 1.0) * 256.0) as i32;
    // Per-column / per-row source pair and fraction (8-bit fixed point).
    let axis = |from: i32, count: u32, origin: i32, n: usize| -> Vec<(usize, usize, u32)> {
        (0..count as i32)
            .map(|i| {
                let local = (from + i - origin) as f32;
                let p = ((local + 0.5) / f as f32 - 0.5).clamp(0.0, (n - 1) as f32);
                let a = p as usize;
                (a, (a + 1).min(n - 1), ((p - a as f32) * 256.0) as u32)
            })
            .collect()
    };
    let cols = axis(dst.x, dst.w, r.x, sw);
    let rows = axis(dst.y, dst.h, r.y, sh);
    for (yi, &(y0, y1, fy)) in rows.iter().enumerate() {
        let base = (dst.y as usize + yi) * stride_px * 4;
        for (xi, &(x0, x1, fx)) in cols.iter().enumerate() {
            let i = base + (dst.x as usize + xi) * 4;
            for c in 0..3 {
                let s = |sy: usize, sx: usize| small[(sy * sw + sx) * 3 + c] as u32;
                let top = (s(y0, x0) * (256 - fx) + s(y0, x1) * fx) >> 8;
                let bot = (s(y1, x0) * (256 - fx) + s(y1, x1) * fx) >> 8;
                let v = ((top * (256 - fy) + bot * fy) >> 8) as i32;
                back[i + c] = if mix >= 256 {
                    v as u8
                } else {
                    let o = back[i + c] as i32;
                    (o + (((v - o) * mix) >> 8)) as u8
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    /// `w` x `h` RGBA, `px(x, y)` gives the pixel.
    fn img(w: u32, h: u32, px: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                v.extend_from_slice(&px(x, y));
            }
        }
        v
    }

    fn red(buf: &[u8], w: u32, x: u32, y: u32) -> u8 {
        buf[((y * w + x) * 4) as usize]
    }

    #[test]
    fn flat_colour_stays_flat() {
        let mut b = img(64, 64, |_, _| [10, 20, 30, 255]);
        let before = b.clone();
        blur_rect(&mut b, 64, 64, r(0, 0, 64, 64), r(0, 0, 64, 64), 8, 1.0, &mut BlurScratch::default());
        assert_eq!(b, before);
    }

    #[test]
    fn hard_edge_softens_but_far_pixels_hold() {
        let mut b = img(64, 8, |x, _| if x < 32 { [0, 0, 0, 255] } else { [255, 0, 0, 255] });
        blur_rect(&mut b, 64, 8, r(0, 0, 64, 8), r(0, 0, 64, 8), 8, 1.0, &mut BlurScratch::default());
        assert!(red(&b, 64, 0, 4) <= 2);
        assert!(red(&b, 64, 63, 4) >= 253);
        let mid = red(&b, 64, 31, 4);
        assert!((20..=235).contains(&mid), "edge pixel was {mid}");
        assert!(red(&b, 64, 30, 4) < red(&b, 64, 33, 4), "gradient must rise left to right");
    }

    #[test]
    fn writes_only_inside_rect_and_clip() {
        let src = img(64, 8, |x, _| if x < 32 { [0, 0, 0, 255] } else { [255, 0, 0, 255] });
        let mut b = src.clone();
        blur_rect(&mut b, 64, 8, r(32, 0, 32, 8), r(0, 0, 64, 8), 8, 1.0, &mut BlurScratch::default());
        for x in 0..32 {
            assert_eq!(red(&b, 64, x, 4), red(&src, 64, x, 4));
        }
        let mut b = src.clone();
        blur_rect(&mut b, 64, 8, r(0, 0, 64, 8), r(0, 0, 32, 8), 8, 1.0, &mut BlurScratch::default());
        for x in 32..64 {
            assert_eq!(red(&b, 64, x, 4), red(&src, 64, x, 4));
        }
        assert_ne!(red(&b, 64, 31, 4), red(&src, 64, 31, 4));
    }

    #[test]
    fn alpha_is_left_alone() {
        let mut b = img(16, 16, |x, _| [x as u8 * 10, 0, 0, 7]);
        blur_rect(&mut b, 16, 16, r(0, 0, 16, 16), r(0, 0, 16, 16), 8, 1.0, &mut BlurScratch::default());
        assert!(b.as_chunks::<4>().0.iter().all(|p| p[3] == 7));
    }

    #[test]
    fn strength_zero_is_a_no_op_and_half_is_between() {
        let src = img(64, 8, |x, _| if x < 32 { [0, 0, 0, 255] } else { [255, 0, 0, 255] });
        let mut none = src.clone();
        blur_rect(&mut none, 64, 8, r(0, 0, 64, 8), r(0, 0, 64, 8), 8, 0.0, &mut BlurScratch::default());
        assert_eq!(none, src);
        let (mut full, mut half) = (src.clone(), src.clone());
        blur_rect(&mut full, 64, 8, r(0, 0, 64, 8), r(0, 0, 64, 8), 8, 1.0, &mut BlurScratch::default());
        blur_rect(&mut half, 64, 8, r(0, 0, 64, 8), r(0, 0, 64, 8), 8, 0.5, &mut BlurScratch::default());
        let (o, f, h) = (red(&src, 64, 30, 4) as i32, red(&full, 64, 30, 4) as i32, red(&half, 64, 30, 4) as i32);
        assert!((h - (o + f) / 2).abs() <= 2, "orig {o} full {f} half {h}");
    }

    #[test]
    fn odd_sizes_and_offscreen_rects_do_not_panic() {
        for radius in [1, 5, 6, 15, 16, 32] {
            let mut b = img(37, 23, |x, y| [(x * 7) as u8, (y * 11) as u8, 3, 255]);
            blur_rect(&mut b, 37, 23, r(-5, -5, 100, 100), r(0, 0, 37, 23), radius, 1.0, &mut BlurScratch::default());
            blur_rect(&mut b, 37, 23, r(30, 20, 50, 50), r(0, 0, 37, 23), radius, 1.0, &mut BlurScratch::default());
            blur_rect(&mut b, 37, 23, r(10, 10, 1, 1), r(0, 0, 37, 23), radius, 1.0, &mut BlurScratch::default());
        }
    }

    #[test]
    fn region_subtract_splits_rects() {
        let out = region_rects(&[(true, r(0, 0, 10, 10)), (false, r(3, 3, 4, 4))]);
        let area: u32 = out.iter().map(|q| q.w * q.h).sum();
        assert_eq!(area, 100 - 16);
        assert!(out.iter().all(|q| q.intersect(&r(3, 3, 4, 4)).is_empty()));
        assert_eq!(region_rects(&[(false, r(0, 0, 5, 5))]), vec![]);
        assert_eq!(region_rects(&[(true, r(0, 0, 0, 5))]), vec![]);
    }

    #[test]
    fn damage_grows_to_cover_touched_footprints_transitively() {
        let (a, b) = (r(0, 0, 100, 40), r(90, 30, 100, 100));
        let d = expand_damage(r(5, 5, 2, 2), &[a, b]);
        assert_eq!(d, a.union(&b));
        assert_eq!(expand_damage(r(500, 500, 2, 2), &[a]), r(500, 500, 2, 2));
        assert!(expand_damage(r(0, 0, 0, 0), &[a]).is_empty());
    }
}
