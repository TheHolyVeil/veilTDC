//! veil-text: font resolution, glyph rasterization and text measurement.
//!
//! One text engine for every Veil surface. Fonts are chosen by *family name*
//! (`font.family = "Inter"`) through an index of what is installed, can be
//! changed live with [`TextSystem::reload`], and always bottom out in a
//! built-in 5x7 bitmap font, so text never fails to appear.
//!
//! Resolution order for each role (UI, mono, icons):
//! 1. an absolute path,
//! 2. an installed family in the font index (see [`fontindex`]),
//! 3. `fc-match`, accepted only if it really returns the family asked for,
//! 4. a built-in preference list (Inter, Noto Sans, DejaVu, ...),
//! 5. the 5x7 bitmap font.

use fontdue::{Font, FontSettings};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use tiny_skia::{Color, Pixmap};

pub mod bitmap5x7;
pub mod fontindex;

use fontindex::FontIndex;

/// The size `FontConfig::size` means when unset. Callers pass sizes relative to
/// this and the configured base size scales them all.
pub const BASE_SIZE: f32 = 14.0;

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
    /// Bitmap-font glyphs are drawn from the top-left of the line box instead
    /// of sitting on a baseline.
    fixed: bool,
}

/// What the user configured (`font = { ... }` in config.lua).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FontConfig {
    pub family: Option<String>,
    pub mono: Option<String>,
    pub icons: Option<String>,
    /// Tried in order for characters the main font lacks. Loaded lazily, so a
    /// big CJK font costs nothing until a glyph actually needs it.
    pub fallback: Vec<String>,
    /// Base size in pixels; 0 means [`BASE_SIZE`].
    pub size: f32,
    pub bold: bool,
}

impl FontConfig {
    /// The wire form used on the IPC `config` event.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "family": self.family,
            "mono": self.mono,
            "icons": self.icons,
            "fallback": self.fallback,
            "size": self.size,
            "weight": if self.bold { "bold" } else { "regular" },
        })
    }

    /// Inverse of [`to_json`](Self::to_json). Missing or mistyped fields keep
    /// their defaults.
    pub fn from_json(v: &serde_json::Value) -> Self {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(String::from);
        Self {
            family: s("family"),
            mono: s("mono"),
            icons: s("icons"),
            fallback: v
                .get("fallback")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            size: v.get("size").and_then(|x| x.as_f64()).map_or(0.0, |x| x as f32),
            bold: v.get("weight").and_then(|x| x.as_str()) == Some("bold"),
        }
    }
}

/// A fallback family, loaded the first time a glyph needs it.
struct FallbackSlot {
    name: String,
    /// `None` = not tried yet; `Some(None)` = tried and not found.
    font: Option<Option<Arc<Font>>>,
}

pub struct TextSystem {
    cfg: FontConfig,
    index: Option<Arc<FontIndex>>,
    /// A background scan that has not finished yet.
    pending: Option<mpsc::Receiver<FontIndex>>,
    ui: Option<Arc<Font>>,
    mono: Option<Arc<Font>>,
    icons: Option<Arc<Font>>,
    resolved: [Option<PathBuf>; 3],
    fallback: Vec<FallbackSlot>,
    glyphs: HashMap<GlyphKey, Arc<CachedGlyph>>,
    /// `cfg.size / BASE_SIZE`, applied to every size passed in.
    scale: f32,
    /// Skip every font (tests, `bitmap_only`).
    bitmap_only: bool,
}

impl Default for TextSystem {
    fn default() -> Self {
        Self::new(&FontConfig::default())
    }
}

const UI_PREFS: &[&str] = &["Inter", "Noto Sans", "DejaVu Sans", "Liberation Sans"];
const UI_PATHS: &[&str] = &[
    "/usr/share/fonts/inter/Inter-Regular.otf",
    "/usr/share/fonts/TTF/Inter-Regular.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
];
const MONO_PREFS: &[&str] = &["JetBrains Mono", "Inconsolata", "DejaVu Sans Mono", "Liberation Mono"];
const MONO_PATHS: &[&str] = &[
    "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/TTF/JetBrainsMonoNLNerdFont-Regular.ttf",
    "/usr/share/fonts/TTF/Inconsolata-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
];
const ICON_PREFS: &[&str] = &[
    "Symbols Nerd Font Mono",
    "Symbols Nerd Font",
    "JetBrainsMono Nerd Font",
    "JetBrainsMonoNL Nerd Font",
    "MesloLGS Nerd Font",
];
const ICON_PATHS: &[&str] = &[
    "/usr/share/fonts/TTF/SymbolsNerdFontMono-Regular.ttf",
    "/usr/share/fonts/TTF/SymbolsNerdFont-Regular.ttf",
    "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
    "/usr/share/fonts/TTF/JetBrainsMonoNLNerdFont-Regular.ttf",
    "/usr/share/fonts/TTF/MesloLGSDZNerdFontPropo-Regular.ttf",
    "/usr/share/fonts/TTF/MesloLGLDZNerdFontMono-Regular.ttf",
];

impl TextSystem {
    /// Production constructor: uses the cached font index right away and
    /// refreshes it on a background thread. If there is no cache yet the first
    /// scan also runs in the background; until it lands, fonts come from the
    /// built-in preference paths. Call [`poll`](Self::poll) from your loop to
    /// pick the finished index up.
    pub fn new(cfg: &FontConfig) -> Self {
        let cached = FontIndex::load_cache();
        let (tx, rx) = mpsc::channel();
        let previous = cached.clone();
        std::thread::spawn(move || {
            let fresh = FontIndex::scan(previous.as_ref());
            if previous.as_ref() != Some(&fresh) {
                fresh.save_cache();
                let _ = tx.send(fresh);
            }
        });
        let mut ts = Self::blank(cfg.clone());
        ts.index = cached.map(Arc::new);
        ts.pending = Some(rx);
        ts.resolve_all();
        ts
    }

    /// Like [`new`](Self::new) with a fixed index and no background work.
    /// `None` resolves only from the built-in preference paths.
    pub fn with_index(cfg: &FontConfig, index: Option<FontIndex>) -> Self {
        let mut ts = Self::blank(cfg.clone());
        ts.index = index.map(Arc::new);
        ts.resolve_all();
        ts
    }

    /// No fonts at all: everything draws with the 5x7 bitmap font.
    pub fn bitmap_only() -> Self {
        let mut ts = Self::blank(FontConfig::default());
        ts.bitmap_only = true;
        ts
    }

    fn blank(cfg: FontConfig) -> Self {
        let scale = Self::scale_of(&cfg);
        Self {
            cfg,
            index: None,
            pending: None,
            ui: None,
            mono: None,
            icons: None,
            resolved: [None, None, None],
            fallback: Vec::new(),
            glyphs: HashMap::with_capacity(512),
            scale,
            bitmap_only: false,
        }
    }

    fn scale_of(cfg: &FontConfig) -> f32 {
        if cfg.size > 0.0 {
            cfg.size / BASE_SIZE
        } else {
            1.0
        }
    }

    pub fn config(&self) -> &FontConfig {
        &self.cfg
    }

    /// The files in use for (UI, mono, icons), for logs and diagnostics.
    pub fn resolved_paths(&self) -> &[Option<PathBuf>; 3] {
        &self.resolved
    }

    /// Switch to a new font configuration live. Returns whether anything
    /// changed (the caller should then redraw). Cheap to call with an
    /// unchanged config.
    pub fn reload(&mut self, cfg: &FontConfig) -> bool {
        if *cfg == self.cfg {
            return false;
        }
        self.cfg = cfg.clone();
        self.scale = Self::scale_of(&self.cfg);
        self.resolve_all();
        true
    }

    /// Install a finished background font scan. Returns true if that changed
    /// which fonts are in use (the caller should then redraw).
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.pending else { return false };
        match rx.try_recv() {
            Ok(index) => {
                self.pending = None;
                self.index = Some(Arc::new(index));
                let before = self.resolved.clone();
                self.resolve_all();
                self.resolved != before
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                false
            }
        }
    }

    // ── resolution ──

    fn resolve_all(&mut self) {
        self.glyphs.clear();
        self.fallback = self
            .cfg
            .fallback
            .iter()
            .map(|n| FallbackSlot { name: n.clone(), font: None })
            .collect();
        if self.bitmap_only {
            self.ui = None;
            self.mono = None;
            self.icons = None;
            self.resolved = [None, None, None];
            return;
        }

        let bold = self.cfg.bold;
        let ui = self.resolve_role(self.cfg.family.as_deref(), UI_PREFS, UI_PATHS, bold);
        let mono = self
            .resolve_role(self.cfg.mono.as_deref(), MONO_PREFS, MONO_PATHS, bold)
            // No mono font installed: the UI font beats the bitmap font.
            .or_else(|| ui.clone());
        let icons = self.resolve_role(self.cfg.icons.as_deref(), ICON_PREFS, ICON_PATHS, false);

        self.resolved = [
            ui.as_ref().map(|r| r.1.clone()),
            mono.as_ref().map(|r| r.1.clone()),
            icons.as_ref().map(|r| r.1.clone()),
        ];
        self.ui = ui.map(|r| r.0);
        self.mono = mono.map(|r| r.0);
        self.icons = icons.map(|r| r.0);
    }

    fn resolve_role(
        &self,
        spec: Option<&str>,
        prefs: &[&str],
        paths: &[&str],
        bold: bool,
    ) -> Option<(Arc<Font>, PathBuf)> {
        if let Some(spec) = spec {
            if let Some(found) = self.load_spec(spec, bold) {
                return Some(found);
            }
            tracing::warn!("font '{spec}' not found; using a default instead");
        }
        if let Some(idx) = &self.index {
            for family in prefs {
                if let Some(found) = idx.find(family, bold).and_then(|e| load_file(&e.path)) {
                    return Some(found);
                }
            }
        }
        paths.iter().find_map(|p| load_file(Path::new(p)))
    }

    /// A user-supplied font: absolute path, installed family, or (only if it
    /// really is that family) whatever `fc-match` says.
    fn load_spec(&self, spec: &str, bold: bool) -> Option<(Arc<Font>, PathBuf)> {
        let p = Path::new(spec);
        if p.is_absolute() {
            return load_file(p);
        }
        if let Some(found) = self
            .index
            .as_ref()
            .and_then(|i| i.find(spec, bold))
            .and_then(|e| load_file(&e.path))
        {
            return Some(found);
        }
        fc_match(spec).and_then(|p| load_file(&p))
    }

    fn fallback_font(&mut self, i: usize) -> Option<Arc<Font>> {
        if self.fallback[i].font.is_none() {
            let name = self.fallback[i].name.clone();
            let loaded = self.load_spec(&name, false).map(|r| r.0);
            if loaded.is_none() {
                tracing::warn!("fallback font '{name}' not found");
            }
            self.fallback[i].font = Some(loaded);
        }
        self.fallback[i].font.clone().flatten()
    }

    /// The font that should draw `ch`: the role's own font if it has the
    /// glyph, then the configured fallbacks, then the icon font, then
    /// whatever else is loaded. `None` means "use the bitmap font".
    fn select_font(&mut self, ch: char, role: Role) -> Option<Arc<Font>> {
        let is_icon = matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD);
        if is_icon {
            if let Some(f) = self.icons.clone().filter(|f| has_glyph(f, ch)) {
                return Some(f);
            }
        }
        let primary = match role {
            Role::Icons => self.icons.clone().or_else(|| self.ui.clone()),
            Role::Mono => self.mono.clone().or_else(|| self.ui.clone()),
            Role::UI => self.ui.clone().or_else(|| self.mono.clone()),
        };
        if let Some(f) = primary.as_ref().filter(|f| has_glyph(f, ch)) {
            return Some(f.clone());
        }
        for i in 0..self.fallback.len() {
            if let Some(f) = self.fallback_font(i).filter(|f| has_glyph(f, ch)) {
                return Some(f);
            }
        }
        for f in [&self.icons, &self.ui, &self.mono].into_iter().flatten() {
            if has_glyph(f, ch) {
                return Some(f.clone());
            }
        }
        // Nobody has it: the primary font draws its "missing" box, which is
        // more honest than silently dropping the character.
        primary
    }

    // ── measuring ──

    fn advance_of(&mut self, ch: char, role: Role, size: f32) -> f32 {
        let key = GlyphKey(ch, size.round() as u16, role);
        if let Some(g) = self.glyphs.get(&key) {
            return g.advance;
        }
        match self.select_font(ch, role) {
            Some(font) => font.metrics(ch, size).advance_width,
            None => 6.0 * bitmap_scale(size) as f32,
        }
    }

    /// Measure width and height of text at the given size.
    pub fn measure(&mut self, text: &str, role: Role, size: f32) -> (f32, f32) {
        let size = size * self.scale;
        if text.is_empty() {
            return (0.0, size);
        }
        let width: f32 = text.chars().map(|ch| self.advance_of(ch, role, size)).sum();
        (width, size * 1.2)
    }

    /// Truncate text with an ellipsis if it exceeds `max_width`.
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

    // ── drawing ──

    fn glyph(&mut self, ch: char, role: Role, size: f32) -> Arc<CachedGlyph> {
        let key = GlyphKey(ch, size.round() as u16, role);
        if let Some(g) = self.glyphs.get(&key) {
            return g.clone();
        }
        let g = Arc::new(match self.select_font(ch, role) {
            Some(font) => {
                let (m, bitmap) = font.rasterize(ch, size);
                CachedGlyph {
                    width: m.width,
                    height: m.height,
                    left: m.xmin,
                    top: m.ymin,
                    advance: m.advance_width,
                    bitmap,
                    fixed: false,
                }
            }
            None => bitmap_glyph(ch, size),
        });
        self.glyphs.insert(key, g.clone());
        g
    }

    /// Lay out `text`: where each glyph's top-left lands, and the total advance.
    /// `y` is the top of the line box; the baseline sits `size` below it.
    fn place(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        role: Role,
        size: f32,
    ) -> (Vec<(i32, i32, Arc<CachedGlyph>)>, f32) {
        let mut cx = x;
        let mut out = Vec::with_capacity(text.len());
        for ch in text.chars() {
            let g = self.glyph(ch, role, size);
            let (gx, gy) = if g.fixed {
                (cx.round() as i32, y.round() as i32)
            } else {
                (
                    (cx + g.left as f32).round() as i32,
                    (y + size - g.top as f32 - g.height as f32).round() as i32,
                )
            };
            cx += g.advance;
            out.push((gx, gy, g));
        }
        (out, cx - x)
    }

    /// Draw text onto a tiny-skia Pixmap. Returns the total advance width.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        x: f32,
        y: f32,
        role: Role,
        size: f32,
        color: Color,
    ) -> f32 {
        let size = size * self.scale;
        let (placed, advance) = self.place(text, x, y, role, size);
        let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
        let pixels = pixmap.pixels_mut();
        for (gx, gy, g) in placed {
            for row in 0..g.height {
                for col in 0..g.width {
                    let a = g.bitmap[row * g.width + col];
                    if a == 0 {
                        continue;
                    }
                    let (px, py) = (gx + col as i32, gy + row as i32);
                    if px < 0 || py < 0 || px >= pw || py >= ph {
                        continue;
                    }
                    let idx = py as usize * pw as usize + px as usize;
                    blend_pixel(&mut pixels[idx], color, (a as f32 / 255.0) * color.alpha());
                }
            }
        }
        advance
    }

    /// Draw text onto a straight (non-premultiplied) RGBA8 buffer, such as the
    /// compositor's framebuffer. Clips to the buffer. Returns the advance.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_rgba(
        &mut self,
        buf: &mut [u8],
        buf_w: u32,
        buf_h: u32,
        text: &str,
        x: f32,
        y: f32,
        role: Role,
        size: f32,
        color: [u8; 4],
    ) -> f32 {
        let size = size * self.scale;
        let (placed, advance) = self.place(text, x, y, role, size);
        let (bw, bh) = (buf_w as i32, buf_h as i32);
        for (gx, gy, g) in placed {
            for row in 0..g.height {
                for col in 0..g.width {
                    let cov = g.bitmap[row * g.width + col] as u32;
                    if cov == 0 {
                        continue;
                    }
                    let (px, py) = (gx + col as i32, gy + row as i32);
                    if px < 0 || py < 0 || px >= bw || py >= bh {
                        continue;
                    }
                    let i = (py as usize * buf_w as usize + px as usize) * 4;
                    let Some(dst) = buf.get_mut(i..i + 4) else { continue };
                    over_straight(dst, color, cov * color[3] as u32 / 255);
                }
            }
        }
        advance
    }
}

fn has_glyph(f: &Font, ch: char) -> bool {
    ch.is_whitespace() || f.lookup_glyph_index(ch) != 0
}

fn bitmap_scale(size: f32) -> usize {
    (size / 7.0).max(1.0).floor() as usize
}

/// A 5x7 bitmap glyph scaled up by an integer factor, as an alpha bitmap.
fn bitmap_glyph(ch: char, size: f32) -> CachedGlyph {
    let s = bitmap_scale(size);
    let (w, h) = (5 * s, 7 * s);
    let mut bitmap = vec![0u8; w * h];
    for (r, &bits) in bitmap5x7::glyph_rows(ch).iter().enumerate() {
        for c in 0..5 {
            if bits & (1 << (4 - c)) != 0 {
                for sy in 0..s {
                    for sx in 0..s {
                        bitmap[(r * s + sy) * w + c * s + sx] = 255;
                    }
                }
            }
        }
    }
    CachedGlyph { width: w, height: h, left: 0, top: 0, advance: 6.0 * s as f32, bitmap, fixed: true }
}

fn load_file(p: &Path) -> Option<(Arc<Font>, PathBuf)> {
    let bytes = std::fs::read(p).ok()?;
    let font = Font::from_bytes(bytes, FontSettings::default()).ok()?;
    Some((Arc::new(font), p.to_path_buf()))
}

/// Ask fontconfig for `spec`, accepting the answer only if it names that
/// family. `fc-match` always returns *something* (its idea of a default), so
/// without this check an unknown family would silently become a different font.
fn fc_match(spec: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\t%{family}", "--", spec])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let (file, families) = text.split_once('\t')?;
    let want = fontindex::norm(spec);
    families
        .split(',')
        .map(fontindex::norm)
        .any(|f| f == want || f.starts_with(&want))
        .then(|| PathBuf::from(file.trim()))
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

    *dst = tiny_skia::PremultipliedColorU8::from_rgba(out_r, out_g, out_b, out_a).unwrap_or(*dst);
}

/// Source-over of `color` at `alpha` (0..=255) onto a straight-alpha RGBA pixel.
fn over_straight(dst: &mut [u8], color: [u8; 4], alpha: u32) {
    let alpha = alpha.min(255);
    if alpha == 0 {
        return;
    }
    let da = dst[3] as u32;
    let out_a = alpha + da * (255 - alpha) / 255;
    if out_a == 0 {
        return;
    }
    for c in 0..3 {
        let s = color[c] as u32 * alpha;
        let d = dst[c] as u32 * da * (255 - alpha) / 255;
        dst[c] = ((s + d) / out_a).min(255) as u8;
    }
    dst[3] = out_a.min(255) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use fontindex::Entry;

    /// Some real font file, if this machine has one. Tests that need real
    /// glyphs skip (with a note) instead of failing on a font-less CI box.
    fn any_font() -> Option<PathBuf> {
        [
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
    }

    fn all_fonts() -> Vec<PathBuf> {
        [
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        ]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
    }

    fn entry(path: &Path, family: &str) -> Entry {
        Entry { path: path.to_path_buf(), mtime: 1, family: family.into(), style: "regular".into() }
    }

    #[test]
    fn text_system_fallback_measure_and_ellipsize() {
        let mut ts = TextSystem::bitmap_only();
        let (w, h) = ts.measure("Veil Desktop", Role::UI, 14.0);
        assert!(w > 0.0);
        assert!(h > 0.0);

        let ell = ts.ellipsize("Long window title that exceeds bounds", Role::UI, 14.0, 50.0);
        assert!(ell.ends_with('…'));
    }

    #[test]
    fn draw_renders_to_pixmap() {
        let mut ts = TextSystem::bitmap_only();
        let mut pixmap = Pixmap::new(200, 40).unwrap();
        pixmap.fill(Color::BLACK);

        let advance = ts.draw(&mut pixmap, "Veil", 10.0, 10.0, Role::UI, 14.0, Color::WHITE);
        assert!(advance > 0.0);
        let drawn = pixmap.pixels().iter().any(|p| p.red() > 0 || p.green() > 0 || p.blue() > 0);
        assert!(drawn);
    }

    #[test]
    fn draw_rgba_blends_onto_a_framebuffer_and_clips() {
        let mut ts = TextSystem::bitmap_only();
        let (w, h) = (80u32, 30u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0, 0, 0, 255]);
        }
        let adv = ts.draw_rgba(&mut buf, w, h, "Hi", 4.0, 4.0, Role::UI, 14.0, [255, 255, 255, 255]);
        assert!(adv > 0.0);
        let lit = buf.as_chunks::<4>().0.iter().filter(|p| p[0] > 200).count();
        assert!(lit > 10, "text should light some pixels, got {lit}");
        assert!(buf.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "an opaque target stays opaque");

        // Off every edge: must clip, not panic or write out of bounds.
        let before = buf.clone();
        ts.draw_rgba(&mut buf, w, h, "clip me", -200.0, -50.0, Role::UI, 14.0, [255, 0, 0, 255]);
        ts.draw_rgba(&mut buf, w, h, "clip me", 5000.0, 5000.0, Role::UI, 14.0, [255, 0, 0, 255]);
        assert_eq!(buf, before);
    }

    #[test]
    fn half_transparent_text_mixes_with_the_background() {
        let mut ts = TextSystem::bitmap_only();
        let (w, h) = (40u32, 20u32);
        let mut buf: Vec<u8> = std::iter::repeat_n([0u8, 0, 100, 255], (w * h) as usize).flatten().collect();
        ts.draw_rgba(&mut buf, w, h, "I", 2.0, 2.0, Role::UI, 14.0, [200, 0, 0, 128]);
        let mixed = buf.as_chunks::<4>().0.iter().find(|p| p[0] > 50 && p[0] < 150).expect("a blended pixel");
        assert!(mixed[2] > 20 && mixed[2] < 100, "blue background partly shows through: {mixed:?}");
    }

    #[test]
    fn family_names_resolve_through_the_index() {
        let Some(font) = any_font() else {
            eprintln!("skip: no system font installed");
            return;
        };
        let idx = FontIndex::from_entries(vec![entry(&font, "Test Family")]);
        let cfg = FontConfig { family: Some("test family".into()), ..Default::default() };
        let ts = TextSystem::with_index(&cfg, Some(idx));
        assert_eq!(ts.resolved_paths()[0].as_deref(), Some(font.as_path()));
    }

    #[test]
    fn absolute_paths_win_over_the_index() {
        let Some(font) = any_font() else {
            eprintln!("skip: no system font installed");
            return;
        };
        let cfg = FontConfig { family: Some(font.to_string_lossy().into()), ..Default::default() };
        let ts = TextSystem::with_index(&cfg, Some(FontIndex::default()));
        assert_eq!(ts.resolved_paths()[0].as_deref(), Some(font.as_path()));
    }

    #[test]
    fn an_unknown_family_falls_back_instead_of_failing() {
        let cfg = FontConfig { family: Some("No Such Family Anywhere 12345".into()), ..Default::default() };
        let mut ts = TextSystem::with_index(&cfg, Some(FontIndex::default()));
        // Whatever it resolved to (a default font or the bitmap font), text works.
        let (w, _) = ts.measure("still renders", Role::UI, 14.0);
        assert!(w > 0.0);
    }

    #[test]
    fn reload_switches_fonts_clears_the_cache_and_reports_changes() {
        let Some(font) = any_font() else {
            eprintln!("skip: no system font installed");
            return;
        };
        let mut ts = TextSystem::with_index(&FontConfig::default(), Some(FontIndex::default()));
        let mut pm = Pixmap::new(100, 30).unwrap();
        ts.draw(&mut pm, "warm", 0.0, 0.0, Role::UI, 14.0, Color::WHITE);
        assert!(!ts.glyphs.is_empty());

        let same = ts.config().clone();
        assert!(!ts.reload(&same), "an unchanged config is a no-op");
        assert!(!ts.glyphs.is_empty(), "and keeps the warm cache");

        let next = FontConfig { family: Some(font.to_string_lossy().into()), ..Default::default() };
        assert!(ts.reload(&next));
        assert!(ts.glyphs.is_empty(), "stale glyphs from the old font must go");
        assert_eq!(ts.resolved_paths()[0].as_deref(), Some(font.as_path()));
    }

    #[test]
    fn base_size_scales_every_measurement() {
        let Some(font) = any_font() else {
            eprintln!("skip: no system font installed");
            return;
        };
        let spec = Some(font.to_string_lossy().to_string());
        let mut small = TextSystem::with_index(&FontConfig { family: spec.clone(), size: 14.0, ..Default::default() }, None);
        let mut big = TextSystem::with_index(&FontConfig { family: spec, size: 28.0, ..Default::default() }, None);
        let (ws, _) = small.measure("Veil Desktop", Role::UI, 14.0);
        let (wb, _) = big.measure("Veil Desktop", Role::UI, 14.0);
        assert!((wb / ws - 2.0).abs() < 0.1, "28px base should be about twice 14px: {ws} vs {wb}");
    }

    #[test]
    fn a_character_nobody_has_still_draws_something() {
        let Some(font) = any_font() else {
            eprintln!("skip: no system font installed");
            return;
        };
        let cfg = FontConfig { family: Some(font.to_string_lossy().into()), ..Default::default() };
        let mut ts = TextSystem::with_index(&cfg, None);
        let (w, _) = ts.measure("\u{10FFFE}", Role::UI, 14.0);
        assert!(w >= 0.0);
        let mut pm = Pixmap::new(40, 30).unwrap();
        ts.draw(&mut pm, "\u{10FFFE}", 2.0, 2.0, Role::UI, 14.0, Color::WHITE);
    }

    #[test]
    fn a_finished_background_scan_is_picked_up_by_poll() {
        let cfg = FontConfig { family: Some("Late Arrival".into()), ..Default::default() };
        let mut ts = TextSystem::with_index(&cfg, None);
        let before = ts.resolved_paths().clone();

        // The new index must point at a font *different* from whatever the
        // default fallback already picked, or there is nothing to observe.
        let Some(font) = all_fonts().into_iter().find(|p| Some(p) != before[0].as_ref()) else {
            eprintln!("skip: need two different system fonts");
            return;
        };

        let (tx, rx) = mpsc::channel();
        ts.pending = Some(rx);
        assert!(!ts.poll(), "nothing has arrived yet");

        tx.send(FontIndex::from_entries(vec![entry(&font, "Late Arrival")])).unwrap();
        assert!(ts.poll(), "the new index changes which font is used");
        assert_eq!(ts.resolved_paths()[0].as_deref(), Some(font.as_path()));
        assert_ne!(*ts.resolved_paths(), before);
        assert!(!ts.poll(), "and it is only applied once");
    }

    #[test]
    fn font_config_survives_the_ipc_json_round_trip() {
        let cfg = FontConfig {
            family: Some("Inter".into()),
            mono: Some("JetBrains Mono".into()),
            icons: None,
            fallback: vec!["Noto Sans CJK JP".into(), "Noto Color Emoji".into()],
            size: 15.0,
            bold: true,
        };
        assert_eq!(FontConfig::from_json(&cfg.to_json()), cfg);
        assert_eq!(FontConfig::from_json(&serde_json::json!({})), FontConfig::default());
        assert_eq!(FontConfig::from_json(&serde_json::json!("junk")), FontConfig::default());
    }
}
