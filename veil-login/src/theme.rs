//! Wallpaper-derived dynamic theming engine for velogin.
//!
//! Samples an active wallpaper image to extract a dominant, vibrant accent color,
//! then generates a harmonized palette (accent, secondary, dark accent, tinted
//! surfaces and background) applied to the Slint `Theme` global at runtime.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn to_slint(self) -> slint::Color {
        slint::Color::from_rgb_u8(self.0, self.1, self.2)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Rgb,
    pub secondary: Rgb,
    pub accent_dark: Rgb,
    pub surface: Rgb,
    pub surface_card: Rgb,
    pub surface_deep: Rgb,
    pub surface_focused: Rgb,
    pub background: Rgb,
    pub text: Rgb,
    pub text_muted: (Rgb, u8),
    pub text_dim: (Rgb, u8),
}

/// Default palette (deep sage green) used when no wallpaper is loaded or when
/// extraction falls back.
pub const DEFAULT_PALETTE: Palette = Palette {
    accent: Rgb(0x9d, 0xb0, 0x8a),
    secondary: Rgb(0xb7, 0xc9, 0xa3),
    accent_dark: Rgb(0x7d, 0x93, 0x69),
    surface: Rgb(0x14, 0x1f, 0x17),
    surface_card: Rgb(0x18, 0x21, 0x1a),
    surface_deep: Rgb(0x12, 0x1a, 0x14),
    surface_focused: Rgb(0x1c, 0x27, 0x1f),
    background: Rgb(0x0b, 0x10, 0x0c),
    text: Rgb(0xe4, 0xe9, 0xdd),
    text_muted: (Rgb(0x9d, 0xb0, 0x8a), 0xaa),
    text_dim: (Rgb(0x9d, 0xb0, 0x8a), 0x66),
};

/// Circular distance between two of the 16 hue bins.
fn bin_dist(a: usize, b: usize) -> usize {
    let d = a.abs_diff(b);
    d.min(16 - d)
}

/// Bins within this many steps (x22.5 degrees) of the dominant one count as
/// the same colour family.
const CAST_RADIUS: usize = 2;
/// A family holding at least this share of all colour weight is a colour
/// *cast* (a Nord-style blue grade, a sepia tint), not a subject.
const CAST_SHARE: f32 = 0.75;
/// Hue bins (x22.5 degrees: 157.5..270, teal through blue to indigo) treated
/// as the "cold grade" that gets subtracted. Deliberately only the cool end: a
/// plain orange or green wallpaper should still give an orange or green UI,
/// it's the blue-tinted ones that make every login screen identical.
const COLD_BINS: std::ops::RangeInclusive<usize> = 7..=11;
/// Under a cast, an outside hue must hold at least this share to be picked.
const MINORITY_SHARE: f32 = 0.03;
/// With nothing outside the cast to pick, the accent is the cast's own colour
/// with its hue turned by this much (180 = the opposite side of the wheel).
const CAST_ROTATION_DEG: f32 = 180.0;

/// Where the accent comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// The average colour of this hue bin, as is.
    Bin(usize),
    /// The average colour of this hue bin with its hue rotated.
    Rotated(usize),
}

/// Choose the accent's source. Normally the heaviest hue bin. But a wallpaper
/// graded into one hue (every Nord wallpaper is blue) makes the heaviest bin
/// say nothing, and the whole UI turns that colour. So when one colour family
/// holds `CAST_SHARE` of the colour, subtract it:
///   1. take the strongest hue outside the family, if it has a real presence;
///   2. otherwise rotate the cast's own hue away from itself.
fn pick_bin(w: &[f32; 16], total: f32) -> Pick {
    let heaviest = |it: &mut dyn Iterator<Item = usize>| {
        it.max_by(|&a, &b| w[a].partial_cmp(&w[b]).unwrap_or(std::cmp::Ordering::Equal))
    };
    let dom = heaviest(&mut (0..16)).unwrap_or(0);
    if !COLD_BINS.contains(&dom) {
        return Pick::Bin(dom);
    }
    let family: f32 = (0..16)
        .filter(|&i| bin_dist(i, dom) <= CAST_RADIUS)
        .map(|i| w[i])
        .sum();
    if family < CAST_SHARE * total {
        return Pick::Bin(dom);
    }
    match heaviest(&mut (0..16).filter(|&i| bin_dist(i, dom) > CAST_RADIUS)) {
        Some(alt) if w[alt] >= MINORITY_SHARE * total => Pick::Bin(alt),
        _ => Pick::Rotated(dom),
    }
}

/// Rotate an RGB colour's hue by `deg`, keeping saturation and value.
fn rotate_hue((r, g, b): (f32, f32, f32), deg: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d <= f32::EPSILON {
        return (r, g, b); // grey: no hue to rotate
    }
    let h = if (max - r).abs() < f32::EPSILON {
        60.0 * (((g - b) / d) % 6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * (((b - r) / d) + 2.0)
    } else {
        60.0 * (((r - g) / d) + 4.0)
    };
    let h = (h + deg).rem_euclid(360.0);
    let (s, v) = (d / max, max);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (r1 + m, g1 + m, b1 + m)
}

/// Extract a harmonized UI palette from `slint::Rgba8Pixel` slice.
pub fn extract_palette(pixels: &[slint::Rgba8Pixel], width: u32, height: u32) -> Palette {
    if pixels.is_empty() || width == 0 || height == 0 {
        return DEFAULT_PALETTE;
    }

    let total_pixels = pixels.len().min((width as usize) * (height as usize));
    let stride = (total_pixels / 4000).max(1);

    // 16 hue bins: [0..16), each 22.5 degrees.
    let mut bin_weights = [0.0f32; 16];
    let mut bin_r = [0.0f32; 16];
    let mut bin_g = [0.0f32; 16];
    let mut bin_b = [0.0f32; 16];
    let mut total_colorful_weight = 0.0f32;

    let mut sampled = 0usize;
    let mut neutral_count = 0usize;
    let mut neutral_r = 0u64;
    let mut neutral_g = 0u64;
    let mut neutral_b = 0u64;

    for i in (0..total_pixels).step_by(stride) {
        let p = pixels[i];
        let r = p.r;
        let g = p.g;
        let b = p.b;
        let a = p.a;
        if a < 128 {
            continue; // Skip transparent
        }
        sampled += 1;

        let rf = r as f32;
        let gf = g as f32;
        let bf = b as f32;
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        let delta = max - min;
        let luma = 0.299 * rf + 0.587 * gf + 0.114 * bf;
        let sat = if max > 0.0 { delta / max } else { 0.0 };

        // Classify as neutral (grey, black, white) or colorful
        if sat < 0.18 || max < 35.0 || min > 240.0 {
            neutral_count += 1;
            neutral_r += r as u64;
            neutral_g += g as u64;
            neutral_b += b as u64;
            continue;
        }

        // Calculate hue in degrees [0, 360)
        let hue = if delta <= 0.0 {
            0.0
        } else if (max - rf).abs() < f32::EPSILON {
            60.0 * (((gf - bf) / delta) % 6.0)
        } else if (max - gf).abs() < f32::EPSILON {
            60.0 * (((bf - rf) / delta) + 2.0)
        } else {
            60.0 * (((rf - gf) / delta) + 4.0)
        };
        let hue = (hue + 360.0) % 360.0;
        let bin = ((hue / 22.5) as usize).min(15);

        // Weight gives preference to vibrant mid-tones
        let brightness_factor = 1.0 - ((luma / 255.0) - 0.55).abs();
        let weight = sat * brightness_factor;

        bin_weights[bin] += weight;
        bin_r[bin] += rf * weight;
        bin_g[bin] += gf * weight;
        bin_b[bin] += bf * weight;
        total_colorful_weight += weight;
    }

    // A mostly-grey wallpaper with one small coloured logo must not pick the
    // logo: colour has to carry a real share of the samples (~4% worth of
    // fully vibrant pixels) before it beats the neutral fallback.
    let accent = if total_colorful_weight > 0.04 * sampled as f32 {
        let (bin, rot) = match pick_bin(&bin_weights, total_colorful_weight) {
            Pick::Bin(i) => (i, 0.0),
            Pick::Rotated(i) => (i, CAST_ROTATION_DEG),
        };
        let w = bin_weights[bin].max(0.0001);
        let (r, g, b) = rotate_hue(
            (bin_r[bin] / w, bin_g[bin] / w, bin_b[bin] / w),
            rot,
        );
        let (r, g, b) = (r.clamp(0.0, 255.0), g.clamp(0.0, 255.0), b.clamp(0.0, 255.0));

        // Ensure proper contrast: ensure luma is between 120 and 210 for good readability
        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
        let scale = if luma < 120.0 {
            135.0 / luma.max(10.0)
        } else if luma > 215.0 {
            200.0 / luma
        } else {
            1.0
        };

        Rgb(
            (r * scale).clamp(20.0, 255.0) as u8,
            (g * scale).clamp(20.0, 255.0) as u8,
            (b * scale).clamp(20.0, 255.0) as u8,
        )
    } else if neutral_count > 0 {
        // High neutral image: a plain grey accent. (This used to lean cool,
        // which made every grey wallpaper's UI faintly blue.)
        let avg_luma = ((neutral_r + neutral_g + neutral_b) / (neutral_count as u64 * 3)) as u8;
        let val = avg_luma.clamp(140, 210);
        Rgb(val, val, val)
    } else {
        DEFAULT_PALETTE.accent
    };

    derive_palette(accent)
}

/// WCAG relative luminance of an sRGB colour.
fn rel_luminance(c: Rgb) -> f32 {
    fn lin(v: u8) -> f32 {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.0) + 0.7152 * lin(c.1) + 0.0722 * lin(c.2)
}

/// WCAG contrast ratio between two colours (1.0 = identical, 21.0 = black on white).
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (rel_luminance(a), rel_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Blend `c` toward `to` by `t` (0.0..=1.0).
fn mix(c: Rgb, to: Rgb, t: f32) -> Rgb {
    let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u8;
    Rgb(m(c.0, to.0), m(c.1, to.1), m(c.2, to.2))
}

/// Minimum contrast the accent must have against the card surface: it is used
/// for text, borders and icons on top of it.
const MIN_ACCENT_CONTRAST: f32 = 4.5;

/// Derive harmonized UI colors from a chosen accent RGB.
pub fn derive_palette(accent: Rgb) -> Palette {
    let Rgb(r, g, b) = accent;

    // surfaces: dark grey with a subtle tint of the accent
    let surf_r = (14 + (r as u16 / 25)) as u8;
    let surf_g = (18 + (g as u16 / 25)) as u8;
    let surf_b = (16 + (b as u16 / 25)) as u8;
    let surface = Rgb(surf_r, surf_g, surf_b);
    let surface_card = Rgb(
        surf_r.saturating_add(4),
        surf_g.saturating_add(4),
        surf_b.saturating_add(4),
    );
    let surface_focused = Rgb(
        surf_r.saturating_add(9),
        surf_g.saturating_add(9),
        surf_b.saturating_add(9),
    );
    let surface_deep = Rgb(
        surf_r.saturating_sub(2),
        surf_g.saturating_sub(2),
        surf_b.saturating_sub(2),
    );
    // background: deep near-black with subtle accent tint
    let background = Rgb(
        (8 + (r as u16 / 40)) as u8,
        (11 + (g as u16 / 40)) as u8,
        (9 + (b as u16 / 40)) as u8,
    );

    // Whatever the wallpaper gave us, the accent must stay readable on the
    // card: nudge it toward white until it clears the contrast bar.
    let mut accent = accent;
    for _ in 0..16 {
        if contrast(accent, surface_card) >= MIN_ACCENT_CONTRAST {
            break;
        }
        accent = mix(accent, Rgb(255, 255, 255), 0.1);
    }
    let Rgb(r, g, b) = accent;

    // secondary: 25% lighter; accent_dark: 20% darker
    let secondary = mix(accent, Rgb(255, 255, 255), 0.25);
    let accent_dark = Rgb(
        ((r as u16 * 80) / 100) as u8,
        ((g as u16 * 80) / 100) as u8,
        ((b as u16 * 80) / 100) as u8,
    );

    // Off-white with a whisper of the accent hue, so text sits in the palette
    // instead of being a fixed green-white on every wallpaper.
    let text = mix(Rgb(0xee, 0xee, 0xee), accent, 0.10);

    Palette {
        accent,
        secondary,
        accent_dark,
        surface,
        surface_card,
        surface_deep,
        surface_focused,
        background,
        text,
        text_muted: (accent, 0xaa),
        text_dim: (accent, 0x66),
    }
}

/// Push a palette into the Slint `Theme` global.
pub fn apply(ui: &crate::LoginWindow, p: &Palette) {
    use slint::ComponentHandle;
    let t = ui.global::<crate::Theme>();
    t.set_accent(p.accent.to_slint());
    t.set_secondary(p.secondary.to_slint());
    t.set_accent_dark(p.accent_dark.to_slint());
    t.set_surface(p.surface.to_slint());
    t.set_surface_card(p.surface_card.to_slint());
    t.set_surface_focused(p.surface_focused.to_slint());
    t.set_surface_deep(p.surface_deep.to_slint());
    t.set_background(p.background.to_slint());
    t.set_text(p.text.to_slint());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(r: u8, g: u8, b: u8, n: usize) -> Vec<slint::Rgba8Pixel> {
        vec![slint::Rgba8Pixel { r, g, b, a: 255 }; n]
    }

    fn mixed(parts: &[((u8, u8, u8), usize)]) -> Vec<slint::Rgba8Pixel> {
        parts
            .iter()
            .flat_map(|&((r, g, b), n)| solid(r, g, b, n))
            .collect()
    }

    const NORD_BLUE: (u8, u8, u8) = (94, 129, 172);
    const NORD_ORANGE: (u8, u8, u8) = (208, 135, 112);

    #[test]
    fn blue_graded_wallpaper_takes_its_accent_from_the_minority_hue() {
        let px = mixed(&[(NORD_BLUE, 90), (NORD_ORANGE, 5), ((90, 90, 90), 5)]);
        let pal = extract_palette(&px, 100, 1);
        assert!(
            pal.accent.0 as i32 > pal.accent.2 as i32 + 30,
            "accent should be warm, got {:?}",
            pal.accent
        );
    }

    #[test]
    fn single_hue_wallpaper_rotates_away_from_its_own_hue() {
        // Nothing but Nord blue: no minority to pick, so the accent is the
        // blue turned to the far side of the wheel (warm), not blue.
        let px = mixed(&[(NORD_BLUE, 100)]);
        let pal = extract_palette(&px, 100, 1);
        assert!(
            pal.accent.0 as i32 > pal.accent.2 as i32 + 30,
            "expected warm, got {:?}",
            pal.accent
        );
    }

    #[test]
    fn multi_coloured_wallpaper_still_follows_its_dominant_hue() {
        // Blue only 55%: not a cast, so no minority override.
        let px = mixed(&[(NORD_BLUE, 55), (NORD_ORANGE, 45)]);
        let pal = extract_palette(&px, 100, 1);
        assert!(pal.accent.2 > pal.accent.0, "blue should win: {:?}", pal.accent);
    }

    #[test]
    fn noise_under_a_cast_does_not_change_the_accent() {
        // 1 orange pixel in 100 is noise: same result as pure blue.
        let pure = extract_palette(&mixed(&[(NORD_BLUE, 100)]), 100, 1);
        let noisy = extract_palette(&mixed(&[(NORD_BLUE, 99), (NORD_ORANGE, 1)]), 100, 1);
        assert_eq!(pure.accent, noisy.accent);
    }

    #[test]
    fn rotate_hue_180_swaps_blue_and_orange_sides() {
        let (r, g, b) = rotate_hue((94.0, 129.0, 172.0), 180.0);
        assert!(r > g && g > b, "blue -> orange: {r} {g} {b}");
        let (r, g, b) = rotate_hue((90.0, 90.0, 90.0), 180.0);
        assert_eq!((r, g, b), (90.0, 90.0, 90.0), "grey is untouched");
    }

    #[test]
    fn default_palette_matches_sage_theme() {
        assert_eq!(DEFAULT_PALETTE.accent, Rgb(0x9d, 0xb0, 0x8a));
        assert_eq!(DEFAULT_PALETTE.secondary, Rgb(0xb7, 0xc9, 0xa3));
        assert_eq!(DEFAULT_PALETTE.surface_deep, Rgb(0x12, 0x1a, 0x14));
    }

    #[test]
    fn extract_palette_from_orange_image() {
        let data = solid(240, 120, 20, 100 * 100);
        let pal = extract_palette(&data, 100, 100);
        assert!(pal.accent.0 > 200, "red must be dominant in orange accent");
        assert!(pal.accent.1 > 90, "green must be present in orange accent");
        assert!(pal.accent.2 < 90, "blue must stay low in orange accent");
        assert!(pal.secondary.0 >= pal.accent.0);
        assert!(pal.secondary.1 >= pal.accent.1);
    }

    #[test]
    fn extract_palette_from_empty_or_degenerate() {
        assert_eq!(extract_palette(&[], 0, 0), DEFAULT_PALETTE);
        assert_eq!(extract_palette(&solid(1, 2, 3, 4), 0, 4), DEFAULT_PALETTE);
    }

    #[test]
    fn small_logo_on_grey_does_not_pick_the_logo() {
        // 99% mid-grey, 1% saturated red.
        let mut data = solid(128, 128, 128, 10_000);
        for p in data.iter_mut().take(100) {
            *p = slint::Rgba8Pixel { r: 230, g: 20, b: 20, a: 255 };
        }
        let pal = extract_palette(&data, 100, 100);
        // grey in, silver/slate out: red must not dominate
        assert!(pal.accent.0 < pal.accent.1 + 40, "picked the tiny red logo: {:?}", pal.accent);
    }

    #[test]
    fn transparent_and_black_images_fall_back_sanely() {
        let clear = vec![slint::Rgba8Pixel { r: 255, g: 0, b: 0, a: 0 }; 400];
        assert_eq!(extract_palette(&clear, 20, 20).accent, DEFAULT_PALETTE.accent);
        let black = solid(0, 0, 0, 400);
        let pal = extract_palette(&black, 20, 20);
        assert!(contrast(pal.accent, pal.surface_card) >= MIN_ACCENT_CONTRAST);
    }

    #[test]
    fn accent_always_clears_contrast_on_the_card() {
        // Sweep the colour cube: whatever accent a wallpaper yields, the
        // derived palette must keep it readable and text must beat the card.
        for r in (0..=255).step_by(51) {
            for g in (0..=255).step_by(51) {
                for b in (0..=255).step_by(51) {
                    let pal = derive_palette(Rgb(r as u8, g as u8, b as u8));
                    assert!(
                        contrast(pal.accent, pal.surface_card) >= MIN_ACCENT_CONTRAST,
                        "accent {:?} unreadable on {:?}",
                        pal.accent,
                        pal.surface_card
                    );
                    assert!(
                        contrast(pal.text, pal.surface_card) >= 7.0,
                        "text unreadable for accent ({r},{g},{b})"
                    );
                    assert!(
                        contrast(pal.text, pal.background) >= 7.0,
                        "text unreadable on background for accent ({r},{g},{b})"
                    );
                }
            }
        }
    }

    #[test]
    fn sage_accent_keeps_the_sage_look() {
        let pal = derive_palette(DEFAULT_PALETTE.accent);
        assert_eq!(pal.accent, DEFAULT_PALETTE.accent);
        // text tint stays within a hair of the hard-coded #e4e9dd
        for (a, b) in [(pal.text.0, 0xe4u8), (pal.text.1, 0xe9), (pal.text.2, 0xdd)] {
            assert!((a as i32 - b as i32).abs() <= 12, "{a} vs {b}");
        }
    }
}
