//! `<mod_key>+Y` wallpaper chooser modal.
//!
//! A big centred panel with a scrolling grid of image previews for every file
//! the wallpaper engine knows about (the files come from the config's
//! `wallpaper.paths`). Arrows / PgUp / PgDn / Home / End navigate, Enter makes
//! the highlighted image the current wallpaper, Esc closes.
//!
//! Choosing only moves the engine's current index (`WallpaperEngine::jump_to`).
//! The rotation timer is never touched, so shuffle keeps running from the new
//! picture. Thumbnails are decoded on a worker thread, lazily, for the rows on
//! screen (plus one row either side), nearest the selection first.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use crate::font5x7::{draw_text, fill_rect, GLYPH_W};

/// Preview size in pixels (16:9).
pub const THUMB_W: u32 = 224;
pub const THUMB_H: u32 = 126;
const CELL_PAD: u32 = 8;
const CAPTION_H: u32 = 22;
const CELL_W: u32 = THUMB_W + CELL_PAD * 2;
const CELL_H: u32 = THUMB_H + CELL_PAD * 2 + CAPTION_H;
const OUTER: u32 = 14;
const HEADER_H: u32 = 30;
const BAR_W: u32 = 8;
const BAR_GAP: u32 = 6;
const TEXT_SCALE: u32 = 2;

/// Where everything sits for a given screen size. Pure, so it is testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub grid_x: i32,
    pub grid_y: i32,
    pub cols: usize,
    pub rows: usize,
}

pub fn geometry(w: u32, h: u32) -> Geometry {
    let max_w = (w * 88 / 100).min(1800);
    let max_h = h * 88 / 100;
    let fixed_w = OUTER * 2 + BAR_GAP + BAR_W;
    let fixed_h = OUTER * 2 + HEADER_H;
    let cols = (max_w.saturating_sub(fixed_w) / CELL_W).clamp(1, 8) as usize;
    let rows = (max_h.saturating_sub(fixed_h) / CELL_H).max(1) as usize;
    let box_w = cols as u32 * CELL_W + fixed_w;
    let box_h = rows as u32 * CELL_H + fixed_h;
    let x = (w as i32 - box_w as i32) / 2;
    let y = (h as i32 - box_h as i32) / 2;
    Geometry {
        x,
        y,
        w: box_w,
        h: box_h,
        grid_x: x + OUTER as i32,
        grid_y: y + (OUTER + HEADER_H) as i32,
        cols,
        rows,
    }
}

/// Where a click landed, as far as the chooser is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A wallpaper tile; carries the engine index to switch to.
    Wallpaper(usize),
    /// On the panel but not on a tile (title, gaps, scrollbar).
    Panel,
    /// Off the panel entirely.
    Outside,
}

const EMPTY_MSG: &str = "NO WALLPAPERS FOUND";

/// The small box shown when there is nothing to choose from: `(x, y, w, h)`.
fn empty_rect(w: u32, h: u32) -> (i32, i32, u32, u32) {
    let text_w = EMPTY_MSG.len() as u32 * (GLYPH_W + 1) * TEXT_SCALE;
    let bw = text_w + OUTER * 4;
    let bh = OUTER * 2 + HEADER_H + 20;
    ((w as i32 - bw as i32) / 2, (h as i32 - bh as i32) / 2, bw, bh)
}

enum Thumb {
    Unloaded,
    Loaded(Vec<u8>), // RGBA, THUMB_W * THUMB_H * 4, opaque
    Failed,
}

struct Item {
    /// Index into `WallpaperEngine::files()` (which may be shuffled).
    file_idx: usize,
    path: PathBuf,
}

pub struct WallpaperChooser {
    /// Display order: sorted by file name, regardless of the engine's order.
    items: Vec<Item>,
    thumbs: Vec<Thumb>,
    /// Highlighted slot in `items`.
    pub selected: usize,
    /// Slot of the wallpaper being shown when the chooser opened.
    current: usize,
    scroll_row: usize,
    cols: usize,
    rows: usize,
    tx: mpsc::Sender<(usize, Option<Vec<u8>>)>,
    rx: mpsc::Receiver<(usize, Option<Vec<u8>>)>,
    /// Bumped per load request; an older worker sees the change and stops.
    generation: Arc<AtomicU64>,
}

impl Drop for WallpaperChooser {
    fn drop(&mut self) {
        // Stop any worker still decoding.
        self.generation.store(u64::MAX, Ordering::Relaxed);
    }
}

impl WallpaperChooser {
    /// `files` is the engine's list as-is; `current_idx` is the engine's
    /// current index into it.
    pub fn new(files: Vec<PathBuf>, current_idx: usize) -> Self {
        let mut items: Vec<Item> = files
            .into_iter()
            .enumerate()
            .map(|(file_idx, path)| Item { file_idx, path })
            .collect();
        items.sort_by_key(|i| {
            i.path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });
        let current = items
            .iter()
            .position(|i| i.file_idx == current_idx)
            .unwrap_or(0);
        let (tx, rx) = mpsc::channel();
        let n = items.len();
        let mut c = Self {
            items,
            thumbs: (0..n).map(|_| Thumb::Unloaded).collect(),
            selected: current,
            current,
            scroll_row: 0,
            cols: 4,
            rows: 3,
            tx,
            rx,
            generation: Arc::new(AtomicU64::new(0)),
        };
        c.ensure_visible();
        c.want_visible();
        c
    }

    /// The engine index of the highlighted wallpaper.
    pub fn selected_file_idx(&self) -> Option<usize> {
        self.items.get(self.selected).map(|i| i.file_idx)
    }

    /// Tell the chooser how many columns/rows the screen shows. Called each
    /// frame before drawing; does nothing unless the shape changed.
    pub fn set_view(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.ensure_visible();
            self.want_visible();
        }
    }

    pub fn move_left(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.after_move();
    }

    pub fn move_right(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
        self.after_move();
    }

    pub fn move_up(&mut self) {
        if self.selected >= self.cols {
            self.selected -= self.cols;
        }
        self.after_move();
    }

    pub fn move_down(&mut self) {
        if self.selected + self.cols < self.items.len() {
            self.selected += self.cols;
        }
        self.after_move();
    }

    pub fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(self.cols * self.rows);
        self.after_move();
    }

    pub fn page_down(&mut self) {
        let last = self.items.len().saturating_sub(1);
        self.selected = (self.selected + self.cols * self.rows).min(last);
        self.after_move();
    }

    pub fn home(&mut self) {
        self.selected = 0;
        self.after_move();
    }

    pub fn end(&mut self) {
        self.selected = self.items.len().saturating_sub(1);
        self.after_move();
    }

    fn after_move(&mut self) {
        let before = self.scroll_row;
        self.ensure_visible();
        if self.scroll_row != before {
            self.want_visible();
        }
    }

    fn total_rows(&self) -> usize {
        self.items.len().div_ceil(self.cols)
    }

    /// Scroll just enough to keep the selected row on screen.
    fn ensure_visible(&mut self) {
        let row = self.selected / self.cols;
        if row < self.scroll_row {
            self.scroll_row = row;
        } else if row >= self.scroll_row + self.rows {
            self.scroll_row = row + 1 - self.rows;
        }
        self.scroll_row = self
            .scroll_row
            .min(self.total_rows().saturating_sub(self.rows));
    }

    /// Start decoding previews for the visible rows ±1, nearest the selection
    /// first. Supersedes any earlier request.
    fn want_visible(&mut self) {
        let first = self.scroll_row.saturating_sub(1) * self.cols;
        let last = ((self.scroll_row + self.rows + 1) * self.cols).min(self.items.len());
        let mut want: Vec<usize> = (first..last)
            .filter(|&p| matches!(self.thumbs[p], Thumb::Unloaded))
            .collect();
        if want.is_empty() {
            return;
        }
        let sel = self.selected as isize;
        want.sort_by_key(|&p| (p as isize - sel).abs());
        let jobs: Vec<(usize, PathBuf)> = want
            .into_iter()
            .map(|p| (p, self.items[p].path.clone()))
            .collect();
        let my_gen = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let generation = Arc::clone(&self.generation);
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new()
            .name("veil-wp-thumbs".into())
            .spawn(move || {
                for (slot, path) in jobs {
                    if generation.load(Ordering::Relaxed) != my_gen {
                        break; // superseded, or the chooser closed
                    }
                    if tx.send((slot, decode_thumb(&path))).is_err() {
                        break;
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("[wallpaper-chooser] could not start loader: {e}");
        }
    }

    /// What is under the pointer, for a `w`×`h` buffer. Uses the same
    /// geometry and scroll position as `draw`.
    pub fn hit_test(&self, x: i32, y: i32, w: u32, h: u32) -> Hit {
        let inside = |rx: i32, ry: i32, rw: u32, rh: u32| {
            x >= rx && y >= ry && x < rx + rw as i32 && y < ry + rh as i32
        };
        if self.items.is_empty() {
            let (ex, ey, ew, eh) = empty_rect(w, h);
            return if inside(ex, ey, ew, eh) { Hit::Panel } else { Hit::Outside };
        }
        let g = geometry(w, h);
        if !inside(g.x, g.y, g.w, g.h) {
            return Hit::Outside;
        }
        if inside(g.grid_x, g.grid_y, g.cols as u32 * CELL_W, g.rows as u32 * CELL_H) {
            let c = (x - g.grid_x) as usize / CELL_W as usize;
            let r = (y - g.grid_y) as usize / CELL_H as usize;
            let slot = (self.scroll_row + r) * g.cols + c;
            if let Some(item) = self.items.get(slot) {
                return Hit::Wallpaper(item.file_idx);
            }
        }
        Hit::Panel
    }

    /// Take finished previews. Returns true if any arrived (needs a redraw).
    pub fn poll_thumbs(&mut self) -> bool {
        let mut got = false;
        while let Ok((slot, rgba)) = self.rx.try_recv() {
            if let Some(t) = self.thumbs.get_mut(slot) {
                *t = rgba.map_or(Thumb::Failed, Thumb::Loaded);
                got = true;
            }
        }
        got
    }

    /// Draw the panel centred on a `w`×`h` RGBA buffer.
    pub fn draw(&self, theme: &veil_config::Theme, back: &mut [u8], w: u32, h: u32) {
        let n = self.items.len();
        if n == 0 {
            let (x, y, bw, bh) = empty_rect(w, h);
            fill_rect(back, w, h, x - 2, y - 2, bw + 4, bh + 4, theme.border);
            fill_rect(back, w, h, x, y, bw, bh, theme.panel_bg);
            let tx = x + (OUTER * 2) as i32;
            draw_text(back, w, h, tx, y + OUTER as i32, TEXT_SCALE, EMPTY_MSG, theme.header);
            let hint_y = y + (OUTER + HEADER_H) as i32;
            draw_text(back, w, h, tx, hint_y, TEXT_SCALE, "ESC CLOSE", theme.text_dim);
            return;
        }
        let g = geometry(w, h);

        fill_rect(back, w, h, g.x - 2, g.y - 2, g.w + 4, g.h + 4, theme.border);
        fill_rect(back, w, h, g.x, g.y, g.w, g.h, theme.panel_bg);

        let title = format!(
            "WALLPAPERS {}/{}   ARROWS MOVE   ENTER SET   ESC CLOSE",
            self.selected + 1,
            n
        );
        draw_text(
            back,
            w,
            h,
            g.x + OUTER as i32,
            g.y + OUTER as i32,
            TEXT_SCALE,
            &title,
            theme.header,
        );

        let max_chars = (THUMB_W / ((GLYPH_W + 1) * TEXT_SCALE)) as usize;
        for r in 0..g.rows {
            for c in 0..g.cols {
                let slot = (self.scroll_row + r) * g.cols + c;
                if slot >= n {
                    break;
                }
                let cx = g.grid_x + (c as u32 * CELL_W) as i32;
                let cy = g.grid_y + (r as u32 * CELL_H) as i32;
                let selected = slot == self.selected;
                if selected {
                    fill_rect(back, w, h, cx, cy, CELL_W, CELL_H, theme.accent);
                    fill_rect(back, w, h, cx + 3, cy + 3, CELL_W - 6, CELL_H - 6, theme.highlight);
                }
                let tx = cx + CELL_PAD as i32;
                let ty = cy + CELL_PAD as i32;
                match &self.thumbs[slot] {
                    Thumb::Loaded(px) => blit_thumb(back, w, h, tx, ty, px),
                    Thumb::Unloaded => {
                        fill_rect(back, w, h, tx, ty, THUMB_W, THUMB_H, theme.text_dim);
                        draw_text(back, w, h, tx + 8, ty + 8, TEXT_SCALE, "LOADING", theme.panel_bg);
                    }
                    Thumb::Failed => {
                        fill_rect(back, w, h, tx, ty, THUMB_W, THUMB_H, theme.text_dim);
                        draw_text(back, w, h, tx + 8, ty + 8, TEXT_SCALE, "CANT LOAD", theme.panel_bg);
                    }
                }
                let mut name = self.items[slot]
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_ascii_uppercase())
                    .unwrap_or_default();
                if slot == self.current {
                    name.insert_str(0, "* ");
                }
                if name.chars().count() > max_chars {
                    name = name.chars().take(max_chars - 2).collect::<String>() + "..";
                }
                let color = if selected { theme.header } else { theme.text };
                draw_text(
                    back,
                    w,
                    h,
                    tx,
                    ty + THUMB_H as i32 + 6,
                    TEXT_SCALE,
                    &name,
                    color,
                );
            }
        }

        // Scrollbar, only when there is something to scroll.
        let total = self.total_rows();
        if total > g.rows {
            let bar_x = g.grid_x + (g.cols as u32 * CELL_W + BAR_GAP) as i32;
            let bar_h = g.rows as u32 * CELL_H;
            fill_rect(back, w, h, bar_x, g.grid_y, BAR_W, bar_h, theme.text_dim);
            let thumb_h = (bar_h * g.rows as u32 / total as u32).max(16);
            let travel = bar_h - thumb_h;
            let off = travel * self.scroll_row as u32 / (total - g.rows) as u32;
            fill_rect(back, w, h, bar_x, g.grid_y + off as i32, BAR_W, thumb_h, theme.accent);
        }
    }
}

/// Copy a THUMB_W×THUMB_H RGBA preview to `(x, y)`, clipped to the buffer.
fn blit_thumb(back: &mut [u8], w: u32, h: u32, x: i32, y: i32, px: &[u8]) {
    if x < 0 || x as u32 + THUMB_W > w {
        return;
    }
    let row_bytes = THUMB_W as usize * 4;
    for ty in 0..THUMB_H as i32 {
        let dy = y + ty;
        if dy < 0 || dy as u32 >= h {
            continue;
        }
        let dst = (dy as usize * w as usize + x as usize) * 4;
        let src = ty as usize * row_bytes;
        if dst + row_bytes <= back.len() && src + row_bytes <= px.len() {
            back[dst..dst + row_bytes].copy_from_slice(&px[src..src + row_bytes]);
        }
    }
}

/// Decode and cover-scale one image to `THUMB_W × THUMB_H` (opaque RGBA).
/// `None` on any decode or IO error.
fn decode_thumb(path: &std::path::Path) -> Option<Vec<u8>> {
    let mut reader = image::ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    reader.limits(limits);
    let img = reader.decode().ok()?.into_rgba8();
    let (iw, ih) = img.dimensions();

    let scale = (THUMB_W as f64 / iw as f64).max(THUMB_H as f64 / ih as f64);
    let sw = ((iw as f64 * scale).ceil() as u32).max(THUMB_W);
    let sh = ((ih as f64 * scale).ceil() as u32).max(THUMB_H);
    let resized = image::imageops::resize(&img, sw, sh, image::imageops::FilterType::Triangle);
    let cropped = image::imageops::crop_imm(
        &resized,
        (sw - THUMB_W) / 2,
        (sh - THUMB_H) / 2,
        THUMB_W,
        THUMB_H,
    );
    let mut rgba = cropped.to_image().into_raw();
    for px in rgba.as_chunks_mut::<4>().0.iter_mut() {
        px[3] = 255;
    }
    Some(rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(n: usize) -> Vec<PathBuf> {
        // Deliberately not in name order, like a shuffled engine list.
        (0..n)
            .map(|i| PathBuf::from(format!("/nonexistent/wall-{:02}.png", (i * 7) % n)))
            .collect()
    }

    #[test]
    fn geometry_fits_the_screen() {
        for (w, h) in [(1920, 1080), (2560, 1440), (1280, 720), (640, 480), (200, 100)] {
            let g = geometry(w, h);
            assert!(g.cols >= 1 && g.rows >= 1);
            if w >= 1280 {
                assert!(g.x >= 0 && g.y >= 0, "{w}x{h}: {g:?}");
                assert!(g.x as u32 + g.w <= w && g.y as u32 + g.h <= h, "{w}x{h}: {g:?}");
            }
        }
        let g = geometry(1920, 1080);
        // 1920*88% = 1689 → (1689-42)/240 = 6 cols; 1080*88% = 950 → (950-58)/164 = 5 rows
        assert_eq!((g.cols, g.rows), (6, 5));
    }

    #[test]
    fn display_order_is_by_name_but_maps_to_engine_index() {
        let f = files(5);
        let c = WallpaperChooser::new(f.clone(), 3);
        // Selected slot is the engine's current file wherever it sorted to.
        assert_eq!(c.selected_file_idx(), Some(3));
        let names: Vec<_> = c.items.iter().map(|i| i.path.clone()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        for item in &c.items {
            assert_eq!(f[item.file_idx], item.path);
        }
    }

    #[test]
    fn navigation_clamps_and_scrolls() {
        let mut c = WallpaperChooser::new(files(20), 0);
        c.set_view(4, 2);
        c.move_up();
        c.move_left();
        assert_eq!(c.selected, 0);
        c.move_down();
        c.move_down();
        assert_eq!((c.selected, c.scroll_row), (8, 1)); // row 2 needs scroll
        c.end();
        assert_eq!(c.selected, 19);
        assert_eq!(c.scroll_row, 3); // 5 rows total, 2 visible
        c.move_right();
        c.move_down();
        assert_eq!(c.selected, 19);
        c.home();
        assert_eq!((c.selected, c.scroll_row), (0, 0));
        c.page_down();
        assert_eq!(c.selected, 8);
        c.page_up();
        assert_eq!(c.selected, 0);
    }

    #[test]
    fn clicks_map_to_engine_indices() {
        let f = files(30);
        let mut c = WallpaperChooser::new(f.clone(), 0);
        let (w, h) = (1920u32, 1080u32);
        let g = geometry(w, h);
        c.set_view(g.cols, g.rows);
        // Centre of the first tile is the first name-sorted file.
        let (cx, cy) = (g.grid_x + 20, g.grid_y + 20);
        let first = c.items[0].file_idx;
        assert_eq!(c.hit_test(cx, cy, w, h), Hit::Wallpaper(first));
        // Second row, second column.
        let (cx, cy) = (g.grid_x + CELL_W as i32 + 20, g.grid_y + CELL_H as i32 + 20);
        assert_eq!(c.hit_test(cx, cy, w, h), Hit::Wallpaper(c.items[g.cols + 1].file_idx));
        // Title bar is panel, far corner is outside.
        assert_eq!(c.hit_test(g.x + 5, g.y + 5, w, h), Hit::Panel);
        assert_eq!(c.hit_test(2, 2, w, h), Hit::Outside);
        // After scrolling, the top row is a later row of items.
        c.end();
        let top = c.scroll_row * g.cols;
        let (cx, cy) = (g.grid_x + 20, g.grid_y + 20);
        assert_eq!(c.hit_test(cx, cy, w, h), Hit::Wallpaper(c.items[top].file_idx));
    }

    #[test]
    fn empty_state_shows_a_message_box() {
        let c = WallpaperChooser::new(Vec::new(), 0);
        let (w, h) = (1280u32, 720u32);
        let (ex, ey, ew, eh) = empty_rect(w, h);
        assert_eq!(c.hit_test(ex + 1, ey + 1, w, h), Hit::Panel);
        assert_eq!(c.hit_test(ex + ew as i32 + 5, ey + eh as i32 + 5, w, h), Hit::Outside);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        c.draw(&veil_config::Theme::for_name(veil_config::ThemeName::Default), &mut buf, w, h);
        assert!(buf.iter().any(|&b| b != 0), "message box was not drawn");
    }

    #[test]
    fn empty_list_is_harmless() {
        let mut c = WallpaperChooser::new(Vec::new(), 0);
        c.move_down();
        c.end();
        assert_eq!(c.selected_file_idx(), None);
    }

    #[test]
    fn draw_stays_in_bounds() {
        let mut c = WallpaperChooser::new(files(12), 2);
        let (w, h) = (1280u32, 720u32);
        let g = geometry(w, h);
        c.set_view(g.cols, g.rows);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        c.draw(&veil_config::Theme::for_name(veil_config::ThemeName::Default), &mut buf, w, h);
    }
}
