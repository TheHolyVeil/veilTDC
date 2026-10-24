//! Palette quantization for the 256- and 16-colour halfblock tiers.
//!
//! Pure functions, no terminal I/O, so they can be tested.

/// xterm 6x6x6 colour-cube channel levels (palette indices 16..=231).
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// VGA/Linux-console default palette: indices 0..=7 normal, 8..=15 bright.
pub const ANSI16: [[u8; 3]; 16] = [
    [0, 0, 0],
    [170, 0, 0],
    [0, 170, 0],
    [170, 85, 0],
    [0, 0, 170],
    [170, 0, 170],
    [0, 170, 170],
    [170, 170, 170],
    [85, 85, 85],
    [255, 85, 85],
    [85, 255, 85],
    [255, 255, 85],
    [85, 85, 255],
    [255, 85, 255],
    [85, 255, 255],
    [255, 255, 255],
];

fn dist2(a: [u8; 3], b: [u8; 3]) -> u32 {
    let d = |x: u8, y: u8| {
        let d = x as i32 - y as i32;
        (d * d) as u32
    };
    d(a[0], b[0]) + d(a[1], b[1]) + d(a[2], b[2])
}

fn cube_index(v: u8) -> usize {
    (0..6)
        .min_by_key(|&i| (CUBE[i] as i32 - v as i32).abs())
        .unwrap_or(0)
}

/// Nearest xterm-256 palette index for `rgb`: the closer of the best colour-cube
/// entry and the best entry on the 24-step grey ramp (indices 232..=255).
/// Indices 0..=15 are never returned: terminals remap them.
pub fn rgb_to_ansi256(rgb: [u8; 3]) -> u8 {
    let (ri, gi, bi) = (cube_index(rgb[0]), cube_index(rgb[1]), cube_index(rgb[2]));
    let cube_rgb = [CUBE[ri], CUBE[gi], CUBE[bi]];
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;

    let avg = (rgb[0] as u32 + rgb[1] as u32 + rgb[2] as u32) / 3;
    // Ramp level for step i is 8 + 10*i.
    let step = ((avg as i32 - 8 + 5) / 10).clamp(0, 23) as u8;
    let g = 8 + 10 * step;
    let grey_rgb = [g, g, g];

    if dist2(rgb, grey_rgb) < dist2(rgb, cube_rgb) {
        232 + step
    } else {
        cube_idx as u8
    }
}

/// 4x4 Bayer threshold for pixel `(x, y)`, centred on zero (-8..=7).
pub fn bayer4(x: usize, y: usize) -> i16 {
    const M: [[i16; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
    M[y & 3][x & 3] - 8
}

/// Nearest of the first `n` entries of [`ANSI16`] (`n` is 8 or 16: the Linux
/// console can only show 8 background colours). `bias` is added to every
/// channel first; pass `bayer4(..) * 5` for ordered dithering, 0 for none.
/// The same pixel always gets the same bias, so frame diffing stays stable.
pub fn rgb_to_ansi16(rgb: [u8; 3], n: usize, bias: i16) -> u8 {
    let c = |v: u8| (v as i16 + bias).clamp(0, 255) as u8;
    let p = [c(rgb[0]), c(rgb[1]), c(rgb[2])];
    (0..n.min(16))
        .min_by_key(|&i| dist2(p, ANSI16[i]))
        .unwrap_or(0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi256_cube_corners_and_greys() {
        assert_eq!(rgb_to_ansi256([255, 0, 0]), 196);
        assert_eq!(rgb_to_ansi256([0, 255, 0]), 46);
        assert_eq!(rgb_to_ansi256([0, 0, 255]), 21);
        assert_eq!(rgb_to_ansi256([255, 255, 255]), 231);
        // Mid-grey uses the ramp, not the coarse cube.
        assert!((232..=255).contains(&rgb_to_ansi256([128, 128, 128])));
        // Never a remappable system colour.
        for v in [0u8, 1, 40, 200, 255] {
            assert!(rgb_to_ansi256([v, v / 2, 255 - v]) >= 16);
        }
    }

    #[test]
    fn ansi16_nearest_and_bg_limit() {
        assert_eq!(rgb_to_ansi16([0, 0, 0], 16, 0), 0);
        assert_eq!(rgb_to_ansi16([255, 255, 255], 16, 0), 15);
        // With only 8 colours available, white maps to light grey (7).
        assert_eq!(rgb_to_ansi16([255, 255, 255], 8, 0), 7);
        assert!(rgb_to_ansi16([200, 10, 10], 8, 0) < 8);
    }

    #[test]
    fn bayer_is_zero_centred_and_tiles() {
        let sum: i16 = (0..4)
            .flat_map(|y| (0..4).map(move |x| bayer4(x, y)))
            .sum();
        assert_eq!(sum, -8 * 16 + 120);
        assert_eq!(bayer4(1, 2), bayer4(5, 6));
    }

    #[test]
    fn dither_spreads_a_flat_mid_tone() {
        // A flat dark-grey region must not collapse to one palette entry.
        let mut seen = std::collections::HashSet::new();
        for y in 0..4 {
            for x in 0..4 {
                seen.insert(rgb_to_ansi16([100, 100, 100], 16, bayer4(x, y) * 5));
            }
        }
        assert!(seen.len() >= 2);
    }
}
