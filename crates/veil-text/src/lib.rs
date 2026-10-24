//! veil-text: Runtime font resolution, glyph rasterization, and text measurement.
//!
//! Provides a unified text API for Veil with automatic font discovery,
//! glyph caching, Nerd Font icon support, and a guaranteed 5x7 bitmap fallback.

use fontdue::{Font, FontSettings};
use std::collections::HashMap;
use std::path::Path;
use tiny_skia::{Color, Pixmap};

pub mod bitmap5x7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    UI,
    Mono,
    Icons,
}

#[derive(Hash, PartialEq, Eq, Clone)]
struct GlyphKey(char, u16, Role);

struct CachedGlyph {
    width: usize,
    height: usize,
    left: i32,
    top: i32,
    advance: f32,
    bitmap: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct FontConfig {
    pub family: Option<String>,
    pub mono: Option<String>,
    pub icons: Option<String>,
    pub fallback: Vec<String>,
    pub size: f32,
}

pub struct TextSystem {
    ui_font: Option<Font>,
    mono_font: Option<Font>,
    icon_font: Option<Font>,
    glyph_cache: HashMap<GlyphKey, CachedGlyph>,
}

impl Default for TextSystem {
    fn default() -> Self {
        Self::new(&FontConfig::default())
    }
}

impl TextSystem {
    pub fn new(cfg: &FontConfig) -> Self {
        let ui_font = resolve_font(cfg.family.as_deref(), &[
            "/usr/share/fonts/inter/Inter-Regular.otf",
            "/usr/share/fonts/TTF/Inter-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        ]);

        let mono_font = resolve_font(cfg.mono.as_deref(), &[
            "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
            "/usr/share/fonts/TTF/JetBrainsMonoNLNerdFont-Regular.ttf",
            "/usr/share/fonts/TTF/Inconsolata-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        ]).or_else(|| ui_font.clone());

        let icon_font = resolve_font(cfg.icons.as_deref(), &[
            "/usr/share/fonts/TTF/SymbolsNerdFontMono-Regular.ttf",
            "/usr/share/fonts/TTF/SymbolsNerdFont-Regular.ttf",
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
            "/usr/share/fonts/TTF/JetBrainsMonoNLNerdFont-Regular.ttf",
            "/usr/share/fonts/TTF/MesloLGSDZNerdFontPropo-Regular.ttf",
            "/usr/share/fonts/TTF/MesloLGLDZNerdFontMono-Regular.ttf",
        ]);

        Self {
            ui_font,
            mono_font,
            icon_font,
            glyph_cache: HashMap::with_capacity(512),
        }
    }

    fn select_font(&self, ch: char, role: Role) -> Option<&Font> {
        // Automatic Nerd Font routing: Private Use Area (U+E000-U+F8FF) or common NF ranges
        let is_icon = matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD);
        if is_icon && self.icon_font.is_some() {
            return self.icon_font.as_ref();
        }

        match role {
            Role::Icons => self.icon_font.as_ref().or(self.ui_font.as_ref()),
            Role::Mono => self.mono_font.as_ref().or(self.ui_font.as_ref()),
            Role::UI => self.ui_font.as_ref().or(self.mono_font.as_ref()),
        }
    }

    /// Measure width and height of text at given size.
    pub fn measure(&mut self, text: &str, role: Role, size: f32) -> (f32, f32) {
        if text.is_empty() {
            return (0.0, size);
        }

        let mut total_width = 0.0f32;
        let line_height = size * 1.2;

        for ch in text.chars() {
            if let Some(font) = self.select_font(ch, role) {
                let metrics = font.metrics(ch, size);
                total_width += metrics.advance_width;
            } else {
                // 5x7 bitmap font fallback: 5px width + 1px spacing scaled
                let scale = (size / 7.0).max(1.0).floor();
                total_width += 6.0 * scale;
            }
        }

        (total_width, line_height)
    }

    /// Truncate text with ellipsis if it exceeds `max_width`.
    pub fn ellipsize(&mut self, text: &str, role: Role, size: f32, max_width: f32) -> String {
        let (w, _) = self.measure(text, role, size);
        if w <= max_width || max_width <= 0.0 {
            return text.to_string();
        }

        let ell = "…";
        let (ell_w, _) = self.measure(ell, role, size);
        let avail = max_width - ell_w;
        if avail <= 0.0 {
            return ell.to_string();
        }

        let mut result = String::new();
        let mut cur_w = 0.0;
        for ch in text.chars() {
            let (cw, _) = self.measure(&ch.to_string(), role, size);
            if cur_w + cw > avail {
                break;
            }
            cur_w += cw;
            result.push(ch);
        }
        result.push_str(ell);
        result
    }

    /// Draw text onto a tiny-skia Pixmap. Returns total advance width.
    pub fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        mut x: f32,
        y: f32,
        role: Role,
        size: f32,
        color: Color,
    ) -> f32 {
        let start_x = x;
        let px_size = size.round() as u16;

        let pw = pixmap.width() as i32;
        let ph = pixmap.height() as i32;

        for ch in text.chars() {
            if let Some(font) = self.select_font(ch, role).cloned() {
                let key = GlyphKey(ch, px_size, role);
                if !self.glyph_cache.contains_key(&key) {
                    let (metrics, bitmap) = font.rasterize(ch, size);
                    self.glyph_cache.insert(
                        key.clone(),
                        CachedGlyph {
                            width: metrics.width,
                            height: metrics.height,
                            left: metrics.xmin,
                            top: metrics.ymin,
                            advance: metrics.advance_width,
                            bitmap,
                        },
                    );
                }

                if let Some(glyph) = self.glyph_cache.get(&key) {
                    let gx = (x + glyph.left as f32).round() as i32;
                    let gy = (y + size - glyph.top as f32 - glyph.height as f32).round() as i32;

                    let pixels = pixmap.pixels_mut();
                    for row in 0..glyph.height {
                        for col in 0..glyph.width {
                            let alpha_byte = glyph.bitmap[row * glyph.width + col];
                            if alpha_byte == 0 {
                                continue;
                            }

                            let px = gx + col as i32;
                            let py = gy + row as i32;
                            if px < 0 || py < 0 || px >= pw || py >= ph {
                                continue;
                            }

                            let idx = py as usize * pw as usize + px as usize;
                            let a_factor = (alpha_byte as f32 / 255.0) * color.alpha();
                            blend_pixel(&mut pixels[idx], color, a_factor);
                        }
                    }
                    x += glyph.advance;
                }
            } else {
                // Tier 6: Draw with built-in 5x7 bitmap font
                let scale = (size / 7.0).max(1.0).floor() as i32;
                let rows = bitmap5x7::glyph_rows(ch);
                let gx = x.round() as i32;
                let gy = y.round() as i32;

                let pixels = pixmap.pixels_mut();
                for (r, &bits) in rows.iter().enumerate() {
                    for c in 0..5 {
                        if (bits & (1 << (4 - c))) != 0 {
                            for sy in 0..scale {
                                for sx in 0..scale {
                                    let px = gx + c * scale + sx;
                                    let py = gy + r as i32 * scale + sy;
                                    if px >= 0 && py >= 0 && px < pw && py < ph {
                                        let idx = py as usize * pw as usize + px as usize;
                                        blend_pixel(&mut pixels[idx], color, color.alpha());
                                    }
                                }
                            }
                        }
                    }
                }
                x += 6.0 * scale as f32;
            }
        }

        x - start_x
    }
}

fn blend_pixel(dst: &mut tiny_skia::PremultipliedColorU8, color: Color, alpha: f32) {
    let src_a = (alpha * 255.0).clamp(0.0, 255.0) as u32;
    let inv_a = 255 - src_a;

    let src_r = ((color.red() * 255.0) * (src_a as f32 / 255.0)) as u32;
    let src_g = ((color.green() * 255.0) * (src_a as f32 / 255.0)) as u32;
    let src_b = ((color.blue() * 255.0) * (src_a as f32 / 255.0)) as u32;

    let dst_r = dst.red() as u32;
    let dst_g = dst.green() as u32;
    let dst_b = dst.blue() as u32;
    let dst_a = dst.alpha() as u32;

    let out_r = ((src_r + ((dst_r * inv_a) / 255)).min(255)) as u8;
    let out_g = ((src_g + ((dst_g * inv_a) / 255)).min(255)) as u8;
    let out_b = ((src_b + ((dst_b * inv_a) / 255)).min(255)) as u8;
    let out_a = ((src_a + ((dst_a * inv_a) / 255)).min(255)) as u8;

    *dst = tiny_skia::PremultipliedColorU8::from_rgba(out_r, out_g, out_b, out_a)
        .unwrap_or(*dst);
}

fn resolve_font(custom: Option<&str>, search_paths: &[&str]) -> Option<Font> {
    if let Some(name_or_path) = custom {
        let p = Path::new(name_or_path);
        if p.exists() {
            if let Ok(bytes) = std::fs::read(p) {
                if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                    return Some(font);
                }
            }
        }
    }

    for path in search_paths {
        let p = Path::new(path);
        if p.exists() {
            if let Ok(bytes) = std::fs::read(p) {
                if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                    return Some(font);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_system_fallback_measure_and_ellipsize() {
        let mut ts = TextSystem::default();
        let (w, h) = ts.measure("Veil Desktop", Role::UI, 14.0);
        assert!(w > 0.0);
        assert!(h > 0.0);

        let ell = ts.ellipsize("Long window title that exceeds bounds", Role::UI, 14.0, 50.0);
        assert!(ell.ends_with('…'));
    }

    #[test]
    fn draw_renders_to_pixmap() {
        let mut ts = TextSystem::default();
        let mut pixmap = Pixmap::new(200, 40).unwrap();
        pixmap.fill(Color::BLACK);

        let advance = ts.draw(
            &mut pixmap,
            "Veil",
            10.0,
            10.0,
            Role::UI,
            14.0,
            Color::WHITE,
        );
        assert!(advance > 0.0);

        // Verify some non-black pixels were rendered
        let has_drawn_pixels = pixmap.pixels().iter().any(|p| p.red() > 0 || p.green() > 0 || p.blue() > 0);
        assert!(has_drawn_pixels);
    }
}
