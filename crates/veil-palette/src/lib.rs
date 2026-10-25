//! veil-palette: one wallpaper-to-colours algorithm for every Veil surface.
//!
//! Two stages:
//!
//! 1. [`sample_rgba`] picks an accent from a wallpaper. This is velogin's
//!    tuned hue-binning (16 bins, colour-cast subtraction, neutral fallbacks)
//!    ported as is, so the login screen and the shell agree about the same
//!    picture. It is deliberately not k-means: a blue-graded wallpaper makes
//!    the biggest cluster say "blue" and every UI turns blue, which is the
//!    problem the cast rules exist to solve.
//! 2. [`from_accent`] derives the eleven UI tokens from that accent in OKLCH
//!    and guarantees contrast: text 7:1, dim text 4.5:1, accent 4.5:1, and the
//!    status colours 4.5:1, on every surface they can sit on.
//!
//! [`mix`] blends two token sets in OKLab for the ~600 ms crossfade.

pub type Rgb = [u8; 3];

/// The sage accent velogin ships with; used when a wallpaper gives no signal.
pub const DEFAULT_ACCENT: Rgb = [0x9d, 0xb0, 0x8a];

/// How long components take to move to a new palette.
pub const CROSSFADE_MS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
    /// Follow the wallpaper: light only when it is very bright.
    Auto,
}

impl Mode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            "auto" => Self::Auto,
            _ => Self::Dark,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Auto => "auto",
        }
    }
}

/// The token set every surface draws from. Names and order match the doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub bg: Rgb,
    pub surface: Rgb,
    pub surface_hi: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    pub text_dim: Rgb,
    pub accent: Rgb,
    pub accent_text: Rgb,
    pub ok: Rgb,
    pub warn: Rgb,
    pub err: Rgb,
}

impl Tokens {
    pub fn entries(&self) -> [(&'static str, Rgb); 11] {
        [
            ("bg", self.bg),
            ("surface", self.surface),
            ("surface_hi", self.surface_hi),
            ("border", self.border),
            ("text", self.text),
            ("text_dim", self.text_dim),
            ("accent", self.accent),
            ("accent_text", self.accent_text),
            ("ok", self.ok),
            ("warn", self.warn),
            ("err", self.err),
        ]
    }

    /// `(name, "#rrggbb")` pairs, for palette.json and the `palette` event.
    pub fn hex_entries(&self) -> [(&'static str, String); 11] {
        self.entries().map(|(n, c)| (n, hex(c)))
    }

    /// Build from `(name, "#rrggbb")` lookups. Unknown or unparsable entries
    /// keep the default for that token.
    pub fn from_hex_lookup<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Self {
        let d = Self::default();
        let pick = |name: &str, dflt: Rgb| get(name).and_then(parse_hex).unwrap_or(dflt);
        Self {
            bg: pick("bg", d.bg),
            surface: pick("surface", d.surface),
            surface_hi: pick("surface_hi", d.surface_hi),
            border: pick("border", d.border),
            text: pick("text", d.text),
            text_dim: pick("text_dim", d.text_dim),
            accent: pick("accent", d.accent),
            accent_text: pick("accent_text", d.accent_text),
            ok: pick("ok", d.ok),
            warn: pick("warn", d.warn),
            err: pick("err", d.err),
        }
    }
}

impl Default for Tokens {
    fn default() -> Self {
        from_accent(DEFAULT_ACCENT, Mode::Dark)
    }
}

pub fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

pub fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.is_ascii() {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

// ── Stage 1: accent from a wallpaper (velogin's algorithm) ──────────────────

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
/// plain orange or green wallpaper should still give an orange or green UI.
const COLD_BINS: std::ops::RangeInclusive<usize> = 7..=11;
/// Under a cast, an outside hue must hold at least this share to be picked.
const MINORITY_SHARE: f32 = 0.03;
/// With nothing outside the cast to pick, the accent is the cast's own colour
/// with its hue turned by this much (180 = the opposite side of the wheel).
const CAST_ROTATION_DEG: f32 = 180.0;
/// Colour has to carry this share of the samples (as fully vibrant pixels)
/// before it beats the neutral fallback: a grey wallpaper with one small
/// coloured logo must not pick the logo.
const MIN_COLOUR_SHARE: f32 = 0.04;
/// At most this many pixels are looked at, whatever the image size.
const SAMPLE_BUDGET: usize = 4000;

enum Pick {
    Bin(usize),
    Rotated(usize),
}

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

/// What a wallpaper tells us before any theming rules apply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub accent: Rgb,
    /// Perceptual lightness of the whole picture, 0.0 (black) to 1.0.
    pub lightness: f32,
}

/// Pick an accent (and measure overall brightness) from tightly packed RGBA8
/// pixels. Looks at about [`SAMPLE_BUDGET`] pixels however big the image is,
/// so it is safe to call on the compositor's tick thread.
pub fn sample_rgba(rgba: &[u8], width: u32, height: u32) -> Sample {
    let total_pixels = (rgba.len() / 4).min(width as usize * height as usize);
    if total_pixels == 0 {
        return Sample { accent: DEFAULT_ACCENT, lightness: 0.0 };
    }
    let stride = (total_pixels / SAMPLE_BUDGET).max(1);

    // 16 hue bins, each 22.5 degrees.
    let mut bin_weights = [0.0f32; 16];
    let mut bin_r = [0.0f32; 16];
    let mut bin_g = [0.0f32; 16];
    let mut bin_b = [0.0f32; 16];
    let mut total_colorful_weight = 0.0f32;

    let mut sampled = 0usize;
    let mut neutral_count = 0usize;
    let (mut neutral_r, mut neutral_g, mut neutral_b) = (0u64, 0u64, 0u64);
    let mut luminance_sum = 0.0f32;

    for i in (0..total_pixels).step_by(stride) {
        let o = i * 4;
        let (r, g, b, a) = (rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3]);
        if a < 128 {
            continue; // transparent
        }
        sampled += 1;
        luminance_sum += rel_luminance([r, g, b]);

        let (rf, gf, bf) = (r as f32, g as f32, b as f32);
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        let delta = max - min;
        let luma = 0.299 * rf + 0.587 * gf + 0.114 * bf;
        let sat = if max > 0.0 { delta / max } else { 0.0 };

        // Neutral (grey, black, white) or colourful?
        if sat < 0.18 || max < 35.0 || min > 240.0 {
            neutral_count += 1;
            neutral_r += r as u64;
            neutral_g += g as u64;
            neutral_b += b as u64;
            continue;
        }

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

        // Prefer vibrant mid-tones.
        let brightness_factor = 1.0 - ((luma / 255.0) - 0.55).abs();
        let weight = sat * brightness_factor;

        bin_weights[bin] += weight;
        bin_r[bin] += rf * weight;
        bin_g[bin] += gf * weight;
        bin_b[bin] += bf * weight;
        total_colorful_weight += weight;
    }

    if sampled == 0 {
        return Sample { accent: DEFAULT_ACCENT, lightness: 0.0 };
    }
    let lightness = (luminance_sum / sampled as f32).cbrt();

    let accent = if total_colorful_weight > MIN_COLOUR_SHARE * sampled as f32 {
        let (bin, rot) = match pick_bin(&bin_weights, total_colorful_weight) {
            Pick::Bin(i) => (i, 0.0),
            Pick::Rotated(i) => (i, CAST_ROTATION_DEG),
        };
        let w = bin_weights[bin].max(0.0001);
        let (r, g, b) = rotate_hue((bin_r[bin] / w, bin_g[bin] / w, bin_b[bin] / w), rot);
        let (r, g, b) = (r.clamp(0.0, 255.0), g.clamp(0.0, 255.0), b.clamp(0.0, 255.0));

        // Keep the accent in a readable brightness band.
        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
        let scale = if luma < 120.0 {
            135.0 / luma.max(10.0)
        } else if luma > 215.0 {
            200.0 / luma
        } else {
            1.0
        };
        [
            (r * scale).clamp(20.0, 255.0) as u8,
            (g * scale).clamp(20.0, 255.0) as u8,
            (b * scale).clamp(20.0, 255.0) as u8,
        ]
    } else if neutral_count > 0 {
        // A plain grey accent. (This used to lean cool, which made every grey
        // wallpaper's UI faintly blue.)
        let avg = ((neutral_r + neutral_g + neutral_b) / (neutral_count as u64 * 3)) as u8;
        let v = avg.clamp(140, 210);
        [v, v, v]
    } else {
        DEFAULT_ACCENT
    };

    Sample { accent, lightness }
}

/// Resolve `Auto` against how bright the wallpaper is.
pub fn resolve_mode(mode: Mode, wallpaper_lightness: f32) -> Mode {
    match mode {
        Mode::Auto if wallpaper_lightness >= 0.72 => Mode::Light,
        Mode::Auto => Mode::Dark,
        m => m,
    }
}

/// Wallpaper in, tokens out.
pub fn from_rgba(rgba: &[u8], width: u32, height: u32, mode: Mode) -> Tokens {
    let s = sample_rgba(rgba, width, height);
    from_accent(s.accent, resolve_mode(mode, s.lightness))
}

// ── Stage 2: tokens from an accent ─────────────────────────────────────────

/// WCAG relative luminance of an sRGB colour.
fn rel_luminance(c: Rgb) -> f32 {
    0.2126 * srgb_to_lin(c[0]) + 0.7152 * srgb_to_lin(c[1]) + 0.0722 * srgb_to_lin(c[2])
}

/// WCAG contrast ratio (1.0 identical, 21.0 black on white).
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (la, lb) = (rel_luminance(a), rel_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Slide `c` along lightness (keeping hue and chroma) until it reaches `min`
/// contrast against every colour in `against`. `lighter` picks the direction.
fn ensure_contrast(c: Rgb, against: &[Rgb], min: f32, lighter: bool) -> Rgb {
    let ok = |c: Rgb| against.iter().all(|&a| contrast(c, a) >= min);
    if ok(c) {
        return c;
    }
    let (mut l, ch, h) = rgb_to_oklch(c);
    loop {
        l = if lighter { (l + 0.01).min(1.0) } else { (l - 0.01).max(0.0) };
        let cand = oklch_to_rgb(l, ch, h);
        if ok(cand) {
            return cand;
        }
        if l >= 1.0 || l <= 0.0 {
            break;
        }
    }
    // Pure white or black always clears the bars against our surfaces.
    if lighter { [255, 255, 255] } else { [0, 0, 0] }
}

/// Derive the eleven tokens from an accent. `Auto` is treated as dark here;
/// resolve it with [`resolve_mode`] first when you have a wallpaper.
pub fn from_accent(accent: Rgb, mode: Mode) -> Tokens {
    let light = matches!(mode, Mode::Light);
    let (_, accent_chroma, hue) = rgb_to_oklch(accent);

    // Surfaces carry a whisper of the accent's hue. A grey accent gives grey
    // surfaces rather than inventing a tint.
    let sc = if accent_chroma < 0.02 { 0.0 } else { (accent_chroma * 0.12).clamp(0.006, 0.03) };
    let tint = |l: f32| oklch_to_rgb(l, sc, hue);

    let (bg, surface, surface_hi, border) = if light {
        (tint(0.975), tint(0.945), tint(0.915), tint(0.82))
    } else {
        (tint(0.14), tint(0.18), tint(0.24), tint(0.33))
    };
    let surfaces = [bg, surface, surface_hi];

    // Accent: readable on the surfaces it is drawn over.
    let accent = ensure_contrast(accent, &[surface, surface_hi], 4.5, !light);

    // Text: start near white (dark mode) or near black (light mode) with a
    // trace of the hue, then slide until it clears 7:1 on every surface.
    let text_hue_chroma = sc.min(0.012);
    let text = if light {
        ensure_contrast(oklch_to_rgb(0.2, text_hue_chroma, hue), &surfaces, 7.0, false)
    } else {
        ensure_contrast(oklch_to_rgb(0.95, text_hue_chroma, hue), &surfaces, 7.0, true)
    };
    let text_dim = if light {
        ensure_contrast(oklch_to_rgb(0.48, text_hue_chroma, hue), &surfaces, 4.5, false)
    } else {
        ensure_contrast(oklch_to_rgb(0.70, text_hue_chroma, hue), &surfaces, 4.5, true)
    };

    // Text on top of an accent fill: near-black or near-white, whichever
    // reads better, falling back to pure black/white if neither reaches 4.5.
    let (dark_t, light_t) = ([12u8, 14, 16], [246u8, 247, 248]);
    let (cd, cl) = (contrast(dark_t, accent), contrast(light_t, accent));
    let accent_text = match (cd >= cl, cd.max(cl) >= 4.5) {
        (true, true) => dark_t,
        (false, true) => light_t,
        (true, false) => [0, 0, 0],
        (false, false) => [255, 255, 255],
    };

    // Status colours: fixed hues, nudged for contrast on the surfaces.
    let status_l = if light { 0.50 } else { 0.76 };
    let status = |h: f32| {
        ensure_contrast(oklch_to_rgb(status_l, 0.15, h), &[surface, surface_hi], 4.5, !light)
    };

    Tokens {
        bg,
        surface,
        surface_hi,
        border,
        text,
        text_dim,
        accent,
        accent_text,
        ok: status(145.0),
        warn: status(80.0),
        err: status(25.0),
    }
}

// ── Mixing ─────────────────────────────────────────────────────────────────

/// Ease-out cubic: fast start, gentle landing.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Eased progress of a crossfade that started `elapsed_ms` ago.
pub fn crossfade_progress(elapsed_ms: u64) -> f32 {
    ease_out(elapsed_ms as f32 / CROSSFADE_MS as f32)
}

/// Blend two colours in OKLab.
pub fn mix_rgb(a: Rgb, b: Rgb, t: f32) -> Rgb {
    if t <= 0.0 {
        return a;
    }
    if t >= 1.0 {
        return b;
    }
    let (la, lb) = (rgb_to_oklab(a), rgb_to_oklab(b));
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    lab_to_rgb(lerp(la[0], lb[0]), lerp(la[1], lb[1]), lerp(la[2], lb[2]))
}

/// Blend two token sets in OKLab.
pub fn mix(a: &Tokens, b: &Tokens, t: f32) -> Tokens {
    let m = |x: Rgb, y: Rgb| mix_rgb(x, y, t);
    Tokens {
        bg: m(a.bg, b.bg),
        surface: m(a.surface, b.surface),
        surface_hi: m(a.surface_hi, b.surface_hi),
        border: m(a.border, b.border),
        text: m(a.text, b.text),
        text_dim: m(a.text_dim, b.text_dim),
        accent: m(a.accent, b.accent),
        accent_text: m(a.accent_text, b.accent_text),
        ok: m(a.ok, b.ok),
        warn: m(a.warn, b.warn),
        err: m(a.err, b.err),
    }
}

// ── OKLab / OKLCH (Björn Ottosson) ─────────────────────────────────────────

fn srgb_to_lin(v: u8) -> f32 {
    let s = v as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

fn lin_to_srgb(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

fn rgb_to_oklab(c: Rgb) -> [f32; 3] {
    let (r, g, b) = (srgb_to_lin(c[0]), srgb_to_lin(c[1]), srgb_to_lin(c[2]));
    let l = (0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_84 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// Linear-light RGB for an OKLab colour, not yet clamped.
fn lab_to_lin(l: f32, a: f32, b: f32) -> [f32; 3] {
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
}

fn lab_to_rgb(l: f32, a: f32, b: f32) -> Rgb {
    let v = lab_to_lin(l, a, b);
    [lin_to_srgb(v[0]), lin_to_srgb(v[1]), lin_to_srgb(v[2])]
}

fn rgb_to_oklch(c: Rgb) -> (f32, f32, f32) {
    let [l, a, b] = rgb_to_oklab(c);
    (l, (a * a + b * b).sqrt(), b.atan2(a).to_degrees().rem_euclid(360.0))
}

/// OKLCH to sRGB. Out-of-gamut requests lose chroma, not lightness or hue.
fn oklch_to_rgb(l: f32, c: f32, h_deg: f32) -> Rgb {
    let h = h_deg.to_radians();
    let lin = |c: f32| lab_to_lin(l, c * h.cos(), c * h.sin());
    let inside = |v: [f32; 3]| v.iter().all(|x| (-0.000_2..=1.000_2).contains(x));
    let c = c.max(0.0);
    if inside(lin(c)) {
        let v = lin(c);
        return [lin_to_srgb(v[0]), lin_to_srgb(v[1]), lin_to_srgb(v[2])];
    }
    let (mut lo, mut hi) = (0.0f32, c);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if inside(lin(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let v = lin(lo);
    [lin_to_srgb(v[0]), lin_to_srgb(v[1]), lin_to_srgb(v[2])]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(r: u8, g: u8, b: u8, n: usize) -> Vec<u8> {
        std::iter::repeat([r, g, b, 255]).take(n).flatten().collect()
    }

    fn mixed(parts: &[((u8, u8, u8), usize)]) -> Vec<u8> {
        parts.iter().flat_map(|&((r, g, b), n)| solid(r, g, b, n)).collect()
    }

    fn accent_of(px: &[u8], w: u32, h: u32) -> Rgb {
        sample_rgba(px, w, h).accent
    }

    const NORD_BLUE: (u8, u8, u8) = (94, 129, 172);
    const NORD_ORANGE: (u8, u8, u8) = (208, 135, 112);

    // ── accent extraction (ported from velogin) ──

    #[test]
    fn blue_graded_wallpaper_takes_its_accent_from_the_minority_hue() {
        let px = mixed(&[(NORD_BLUE, 90), (NORD_ORANGE, 5), ((90, 90, 90), 5)]);
        let a = accent_of(&px, 100, 1);
        assert!(a[0] as i32 > a[2] as i32 + 30, "accent should be warm, got {a:?}");
    }

    #[test]
    fn single_hue_wallpaper_rotates_away_from_its_own_hue() {
        let a = accent_of(&mixed(&[(NORD_BLUE, 100)]), 100, 1);
        assert!(a[0] as i32 > a[2] as i32 + 30, "expected warm, got {a:?}");
    }

    #[test]
    fn multi_coloured_wallpaper_still_follows_its_dominant_hue() {
        let a = accent_of(&mixed(&[(NORD_BLUE, 55), (NORD_ORANGE, 45)]), 100, 1);
        assert!(a[2] > a[0], "blue should win: {a:?}");
    }

    #[test]
    fn noise_under_a_cast_does_not_change_the_accent() {
        let pure = accent_of(&mixed(&[(NORD_BLUE, 100)]), 100, 1);
        let noisy = accent_of(&mixed(&[(NORD_BLUE, 99), (NORD_ORANGE, 1)]), 100, 1);
        assert_eq!(pure, noisy);
    }

    #[test]
    fn rotate_hue_180_swaps_blue_and_orange_sides() {
        let (r, g, b) = rotate_hue((94.0, 129.0, 172.0), 180.0);
        assert!(r > g && g > b, "blue -> orange: {r} {g} {b}");
        assert_eq!(rotate_hue((90.0, 90.0, 90.0), 180.0), (90.0, 90.0, 90.0), "grey untouched");
    }

    #[test]
    fn orange_image_gives_an_orange_accent() {
        let a = accent_of(&solid(240, 120, 20, 100 * 100), 100, 100);
        assert!(a[0] > 200 && a[1] > 90 && a[2] < 90, "got {a:?}");
    }

    #[test]
    fn empty_or_degenerate_images_fall_back() {
        assert_eq!(accent_of(&[], 0, 0), DEFAULT_ACCENT);
        assert_eq!(accent_of(&solid(1, 2, 3, 4), 0, 4), DEFAULT_ACCENT);
    }

    #[test]
    fn small_logo_on_grey_does_not_pick_the_logo() {
        let mut data = solid(128, 128, 128, 10_000);
        for px in data.chunks_exact_mut(4).take(100) {
            px.copy_from_slice(&[230, 20, 20, 255]);
        }
        let a = accent_of(&data, 100, 100);
        assert!(a[0] < a[1] + 40, "picked the tiny red logo: {a:?}");
    }

    #[test]
    fn transparent_and_black_images_fall_back_sanely() {
        let clear: Vec<u8> = std::iter::repeat([255u8, 0, 0, 0]).take(400).flatten().collect();
        assert_eq!(accent_of(&clear, 20, 20), DEFAULT_ACCENT);
        let t = from_rgba(&solid(0, 0, 0, 400), 20, 20, Mode::Dark);
        assert!(contrast(t.accent, t.surface) >= 4.5);
    }

    #[test]
    fn extraction_is_deterministic() {
        let px = mixed(&[(NORD_BLUE, 60), (NORD_ORANGE, 30), ((20, 20, 20), 10)]);
        assert_eq!(from_rgba(&px, 100, 1, Mode::Dark), from_rgba(&px, 100, 1, Mode::Dark));
    }

    // ── tokens ──

    fn check_contrast_guarantees(accent: Rgb, mode: Mode) {
        let t = from_accent(accent, mode);
        for (name, s) in [("bg", t.bg), ("surface", t.surface), ("surface_hi", t.surface_hi)] {
            assert!(contrast(t.text, s) >= 7.0, "{mode:?} text on {name} for {accent:?}");
            assert!(contrast(t.text_dim, s) >= 4.5, "{mode:?} text_dim on {name} for {accent:?}");
        }
        for s in [t.surface, t.surface_hi] {
            assert!(contrast(t.accent, s) >= 4.5, "{mode:?} accent on {s:?} for {accent:?}");
            for (n, c) in [("ok", t.ok), ("warn", t.warn), ("err", t.err)] {
                assert!(contrast(c, s) >= 4.5, "{mode:?} {n} on {s:?} for {accent:?}");
            }
        }
        assert!(contrast(t.accent_text, t.accent) >= 4.5, "{mode:?} accent_text for {accent:?}");
    }

    #[test]
    fn contrast_guarantees_hold_across_the_colour_cube() {
        for r in (0..=255).step_by(51) {
            for g in (0..=255).step_by(51) {
                for b in (0..=255).step_by(51) {
                    for mode in [Mode::Dark, Mode::Light] {
                        check_contrast_guarantees([r as u8, g as u8, b as u8], mode);
                    }
                }
            }
        }
    }

    #[test]
    fn sage_accent_keeps_the_sage_look() {
        let t = from_accent(DEFAULT_ACCENT, Mode::Dark);
        assert_eq!(t.accent, DEFAULT_ACCENT, "an already-readable accent is not touched");
    }

    #[test]
    fn dark_surfaces_are_darker_than_light_surfaces() {
        let d = from_accent([200, 120, 60], Mode::Dark);
        let l = from_accent([200, 120, 60], Mode::Light);
        assert!(rel_luminance(d.surface) < 0.05);
        assert!(rel_luminance(l.surface) > 0.7);
    }

    #[test]
    fn grey_accent_gives_grey_surfaces() {
        let t = from_accent([150, 150, 150], Mode::Dark);
        let s = t.surface;
        assert!((s[0] as i32 - s[2] as i32).abs() <= 2, "tinted a grey wallpaper: {s:?}");
    }

    #[test]
    fn auto_mode_follows_wallpaper_brightness() {
        assert_eq!(resolve_mode(Mode::Auto, 0.2), Mode::Dark);
        assert_eq!(resolve_mode(Mode::Auto, 0.9), Mode::Light);
        assert_eq!(resolve_mode(Mode::Dark, 0.99), Mode::Dark);
        let bright = from_rgba(&solid(240, 235, 225, 400), 20, 20, Mode::Auto);
        let dark = from_rgba(&solid(20, 25, 30, 400), 20, 20, Mode::Auto);
        assert!(rel_luminance(bright.bg) > 0.7 && rel_luminance(dark.bg) < 0.05);
    }

    // ── mixing and encoding ──

    #[test]
    fn mix_hits_both_ends_and_lands_between() {
        let a = from_accent([200, 80, 60], Mode::Dark);
        let b = from_accent([60, 120, 200], Mode::Dark);
        assert_eq!(mix(&a, &b, 0.0), a);
        assert_eq!(mix(&a, &b, 1.0), b);
        let mid = mix(&a, &b, 0.5);
        assert_ne!(mid.accent, a.accent);
        assert_ne!(mid.accent, b.accent);
    }

    #[test]
    fn easing_is_monotonic_and_bounded() {
        assert_eq!(ease_out(0.0), 0.0);
        assert_eq!(ease_out(1.0), 1.0);
        assert!(ease_out(0.5) > 0.5, "ease-out leads");
        assert_eq!(crossfade_progress(10_000), 1.0);
        let mut prev = 0.0;
        for i in 0..=20 {
            let v = ease_out(i as f32 / 20.0);
            assert!(v >= prev);
            prev = v;
        }
    }

    #[test]
    fn hex_round_trips() {
        let t = from_accent([123, 45, 200], Mode::Dark);
        let entries = t.hex_entries();
        let back = Tokens::from_hex_lookup(|k| entries.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str()));
        assert_eq!(back, t);
        assert_eq!(parse_hex("#9db08a"), Some(DEFAULT_ACCENT));
        assert_eq!(parse_hex("9db08a"), Some(DEFAULT_ACCENT));
        assert_eq!(parse_hex("nope"), None);
        assert_eq!(parse_hex("#12345"), None);
    }
}
