//! Static wallpaper engine.
//!
//! Decodes and scales on a worker thread into one opaque RGBA layer per
//! distinct monitor size, then hands the layers to the compositor, which
//! copies the layer in place of the solid background fill (a memcpy, cheaper
//! than the fill it replaces). Rotation just starts another load; the old
//! layer keeps showing until the new one is ready, so a 5K JPEG never stalls
//! the composite loop.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use image::imageops::{self, FilterType};
use image::{Rgba, RgbaImage};
use veil_config::{WallpaperConfig, WallpaperFit};

/// Longest side we'll decode. Far past any real display, short of a decode bomb.
const MAX_DIM: u32 = 16_384;
/// Cap on files taken from directories, so a huge folder can't balloon state.
const MAX_FILES: usize = 512;

/// One scaled, opaque RGBA image sized exactly for a monitor.
pub struct Layer {
    pub size: (u32, u32),
    pub rgba: Vec<u8>,
}

struct Loaded {
    generation: u64,
    layers: Vec<Layer>,
}

#[derive(Default)]
pub struct WallpaperEngine {
    cfg: Option<WallpaperConfig>,
    files: Vec<PathBuf>,
    idx: usize,
    next_rotate: Option<Instant>,
    /// Bumped per request; a result from an older request is dropped.
    generation: u64,
    rx: Option<mpsc::Receiver<Loaded>>,
}

impl WallpaperEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any wallpaper file is configured.
    pub fn is_active(&self) -> bool {
        !self.files.is_empty()
    }

    /// Apply a (re)loaded config. Returns true if it changed, i.e. the caller
    /// should `request` a fresh load (or clear the layers if now inactive).
    pub fn configure(&mut self, cfg: Option<WallpaperConfig>) -> bool {
        if cfg == self.cfg {
            return false;
        }
        self.files = cfg
            .as_ref()
            .map_or_else(Vec::new, |c| expand_paths(&c.paths));
        if cfg.as_ref().is_some_and(|c| c.shuffle) {
            shuffle(&mut self.files);
        }
        self.idx = 0;
        self.generation += 1; // orphan any in-flight load of the old config
        self.rx = None;
        self.next_rotate = cfg
            .as_ref()
            .filter(|c| c.interval_secs > 0 && self.files.len() > 1)
            .map(|c| Instant::now() + Duration::from_secs(c.interval_secs));
        self.cfg = cfg;
        true
    }

    /// Start loading the current file for `sizes`. `bg` colours letterbox bars.
    pub fn request(&mut self, sizes: Vec<(u32, u32)>, bg: [u8; 4]) {
        let Some(cfg) = &self.cfg else { return };
        if self.files.is_empty() || sizes.is_empty() {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let files = self.files.clone();
        let (start, fit) = (self.idx, cfg.fit);
        let spawned = std::thread::Builder::new()
            .name("veil-wallpaper".into())
            .spawn(move || {
                // Try the chosen file, then the rest in order: one corrupt
                // image must not blank the wallpaper.
                let mut layers = Vec::new();
                for k in 0..files.len() {
                    let path = &files[(start + k) % files.len()];
                    match load_scaled(path, &sizes, fit, bg) {
                        Ok(l) => {
                            layers = l;
                            break;
                        }
                        Err(e) => eprintln!("[wallpaper] {}: {e}", path.display()),
                    }
                }
                let _ = tx.send(Loaded { generation, layers });
            });
        if let Err(e) = spawned {
            eprintln!("[wallpaper] could not start loader thread: {e}");
            self.rx = None;
        }
    }

    /// Call every tick. Returns freshly loaded layers once, and starts the
    /// next rotation when due. `sizes` is only evaluated when a load starts.
    pub fn tick(
        &mut self,
        now: Instant,
        sizes: impl FnOnce() -> Vec<(u32, u32)>,
        bg: [u8; 4],
    ) -> Option<Vec<Layer>> {
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(l) if l.generation == self.generation => {
                    self.rx = None;
                    return Some(l.layers);
                }
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => self.rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.rx.is_none() {
            if let (Some(due), Some(cfg)) = (self.next_rotate, &self.cfg) {
                if now >= due {
                    self.idx = (self.idx + 1) % self.files.len();
                    self.next_rotate = Some(now + Duration::from_secs(cfg.interval_secs));
                    self.request(sizes(), bg);
                }
            }
        }
        None
    }
}

/// Files and directories (non-recursive) to a flat, sorted-per-directory list.
fn expand_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut dir: Vec<PathBuf> = std::fs::read_dir(p)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|f| is_image_name(f) && f.is_file())
                .collect();
            dir.sort();
            out.extend(dir);
        } else if p.is_file() {
            out.push(p.clone());
        } else {
            eprintln!("[wallpaper] {} not found, skipping", p.display());
        }
        if out.len() >= MAX_FILES {
            out.truncate(MAX_FILES);
            break;
        }
    }
    out
}

fn is_image_name(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg"))
}

/// Fisher-Yates driven by xorshift64, seeded from the clock. Only picks an
/// order of pictures, so no need for a real RNG dependency.
fn shuffle<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64)
        | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

fn load_scaled(
    path: &Path,
    sizes: &[(u32, u32)],
    fit: WallpaperFit,
    bg: [u8; 4],
) -> Result<Vec<Layer>, String> {
    let mut reader = image::ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    reader.limits(limits);
    let src = reader.decode().map_err(|e| e.to_string())?.into_rgba8();

    let mut layers: Vec<Layer> = Vec::new();
    for &size in sizes {
        if size.0 == 0 || size.1 == 0 || layers.iter().any(|l| l.size == size) {
            continue;
        }
        layers.push(Layer {
            size,
            rgba: scale(&src, size, fit, bg),
        });
    }
    Ok(layers)
}

fn scale(src: &RgbaImage, (w, h): (u32, u32), fit: WallpaperFit, bg: [u8; 4]) -> Vec<u8> {
    let (iw, ih) = src.dimensions();
    let (fw, fh, fiw, fih) = (w as f64, h as f64, iw as f64, ih as f64);
    let mut out = match fit {
        WallpaperFit::Stretch => imageops::resize(src, w, h, FilterType::Triangle),
        WallpaperFit::Fill => {
            let s = (fw / fiw).max(fh / fih);
            let sw = ((fiw * s).ceil() as u32).max(w);
            let sh = ((fih * s).ceil() as u32).max(h);
            let r = imageops::resize(src, sw, sh, FilterType::Triangle);
            imageops::crop_imm(&r, (sw - w) / 2, (sh - h) / 2, w, h).to_image()
        }
        WallpaperFit::Fit => {
            let s = (fw / fiw).min(fh / fih);
            let sw = ((fiw * s).round() as u32).clamp(1, w);
            let sh = ((fih * s).round() as u32).clamp(1, h);
            let r = imageops::resize(src, sw, sh, FilterType::Triangle);
            let mut canvas = RgbaImage::from_pixel(w, h, Rgba(bg));
            imageops::overlay(
                &mut canvas,
                &r,
                i64::from((w - sw) / 2),
                i64::from((h - sh) / 2),
            );
            canvas
        }
    };
    // The composite buffer is opaque; never let a PNG's alpha through.
    for px in out.pixels_mut() {
        px.0[3] = 255;
    }
    out.into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, c: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(c))
    }

    #[test]
    fn scale_output_is_exact_size_and_opaque() {
        let src = solid(40, 10, [10, 20, 30, 0]);
        for fit in [WallpaperFit::Fill, WallpaperFit::Fit, WallpaperFit::Stretch] {
            let out = scale(&src, (16, 9), fit, [1, 2, 3, 255]);
            assert_eq!(out.len(), 16 * 9 * 4, "{fit:?}");
            assert!(out.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "{fit:?}");
        }
    }

    #[test]
    fn fit_letterboxes_with_background() {
        // 2:1 image into a square: bars top and bottom keep the bg colour.
        let src = solid(20, 10, [200, 0, 0, 255]);
        let out = scale(&src, (10, 10), WallpaperFit::Fit, [7, 8, 9, 255]);
        assert_eq!(&out[0..4], &[7, 8, 9, 255]);
        let mid = (5 * 10 + 5) * 4;
        assert_eq!(&out[mid..mid + 3], &[200, 0, 0]);
    }

    #[test]
    fn shuffle_keeps_every_element() {
        let mut v: Vec<u32> = (0..32).collect();
        shuffle(&mut v);
        v.sort_unstable();
        assert_eq!(v, (0..32).collect::<Vec<_>>());
    }

    #[test]
    fn image_name_filter() {
        assert!(is_image_name(Path::new("a.PNG")));
        assert!(is_image_name(Path::new("a.jpeg")));
        assert!(!is_image_name(Path::new("a.gif")));
        assert!(!is_image_name(Path::new("notes")));
    }
}
