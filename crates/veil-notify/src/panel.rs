//! The history panel: layout, drawing and hit-testing, in surface coordinates.
//! Fixed-height rows (app, one-line summary, age, a remove box), a header with
//! the title, a do-not-disturb toggle, "Clear" and a close box. The row area
//! scrolls; rows are laid out first and the opaque header is drawn over them.

use tiny_skia::Pixmap;
use veil_ui::{fill_rect, fill_rounded_rect, stroke_rounded_rect, Role, TextSystem, Theme};

use crate::layout::Rect;
use crate::model::{Notification, Urgency};

#[derive(Clone, Debug)]
pub struct PanelStyle {
    pub width: u32,
    pub pad: f32,
    pub radius: f32,
    pub header_h: u32,
    pub row_h: u32,
    pub max_height: u32,
    pub empty_h: u32,
    pub title_size: f32,
    pub text_size: f32,
    pub small_size: f32,
}

impl Default for PanelStyle {
    fn default() -> Self {
        PanelStyle { width: 380, pad: 12.0, radius: 12.0, header_h: 44, row_h: 54, max_height: 560, empty_h: 64, title_size: 15.0, text_size: 13.0, small_size: 11.0 }
    }
}

/// What a row shows; built from a history entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: u32,
    pub app: String,
    pub summary: String,
    pub created_ms: u64,
    pub unread: bool,
    pub urgency: Urgency,
}

impl Row {
    pub fn from_notification(n: &Notification) -> Row {
        Row { id: n.id, app: n.app_name.clone(), summary: n.summary.clone(), created_ms: n.created_ms, unread: !n.read, urgency: n.urgency }
    }
}

/// "now", "5m", "3h", "2d": no timezone needed.
pub fn age_label(created_ms: u64, now_ms: u64) -> String {
    let s = now_ms.saturating_sub(created_ms) / 1000;
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

#[derive(Clone, Debug)]
pub struct RowLayout {
    pub id: u32,
    pub rect: Rect,
    pub remove: Rect,
    app: String,
    summary: String,
    age: String,
    unread: bool,
    urgency: Urgency,
}

#[derive(Clone, Debug)]
pub struct PanelLayout {
    pub width: u32,
    pub height: u32,
    pub close: Rect,
    pub dnd: Rect,
    pub clear: Rect,
    pub rows: Vec<RowLayout>,
    /// Scroll offset actually applied (clamped).
    pub scroll: i32,
    pub max_scroll: i32,
    pub empty: bool,
    dnd_on: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHit {
    Close,
    Dnd,
    Clear,
    Remove(u32),
    Row(u32),
}

pub fn layout(rows: &[Row], dnd_on: bool, scroll: i32, now_ms: u64, st: &PanelStyle, text: &mut TextSystem) -> PanelLayout {
    let (w, head) = (st.width as i32, st.header_h as i32);
    let area_h = if rows.is_empty() { st.empty_h as i32 } else { (rows.len() as i32 * st.row_h as i32).min(st.max_height as i32 - head) };
    let height = (head + area_h) as u32;
    let total = rows.len() as i32 * st.row_h as i32;
    let max_scroll = (total - area_h).max(0);
    let scroll = scroll.clamp(0, max_scroll);

    let pad = st.pad as i32;
    let close = Rect { x: w - pad - 18, y: (head - 18) / 2, w: 18, h: 18 };
    let clear_w = text.measure("Clear", Role::UI, st.text_size).0.ceil() as u32 + 16;
    let clear = Rect { x: close.x - 8 - clear_w as i32, y: (head - 26) / 2, w: clear_w, h: 26 };
    let dnd_w = text.measure("Do not disturb", Role::UI, st.small_size).0.ceil() as u32 + 22;
    let dnd = Rect { x: clear.x - 8 - dnd_w as i32, y: (head - 26) / 2, w: dnd_w, h: 26 };

    let text_w = (w - 2 * pad - 40 - 30) as f32;
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let y = head + i as i32 * st.row_h as i32 - scroll;
        if y + st.row_h as i32 <= head || y >= height as i32 {
            continue;
        }
        let rect = Rect { x: 0, y, w: st.width, h: st.row_h };
        let remove = Rect { x: w - pad - 18, y: y + (st.row_h as i32 - 18) / 2, w: 18, h: 18 };
        out.push(RowLayout {
            id: r.id,
            rect,
            remove,
            app: text.ellipsize(&r.app, Role::UI, st.small_size, text_w),
            summary: text.ellipsize(&r.summary, Role::UI, st.text_size, text_w),
            age: age_label(r.created_ms, now_ms),
            unread: r.unread,
            urgency: r.urgency,
        });
    }
    PanelLayout { width: st.width, height, close, dnd, clear, rows: out, scroll, max_scroll, empty: rows.is_empty(), dnd_on }
}

/// What is under the surface-local point; `None` outside the panel.
pub fn hit(l: &PanelLayout, x: i32, y: i32, st: &PanelStyle) -> Option<PanelHit> {
    if x < 0 || y < 0 || x >= l.width as i32 || y >= l.height as i32 {
        return None;
    }
    if y < st.header_h as i32 {
        return [(l.close, PanelHit::Close), (l.clear, PanelHit::Clear), (l.dnd, PanelHit::Dnd)]
            .into_iter()
            .find(|(r, _)| r.contains(x, y))
            .map(|(_, h)| h);
    }
    let row = l.rows.iter().find(|r| r.rect.contains(x, y))?;
    Some(if row.remove.contains(x, y) { PanelHit::Remove(row.id) } else { PanelHit::Row(row.id) })
}

pub fn draw(l: &PanelLayout, st: &PanelStyle, theme: &Theme, text: &mut TextSystem, hover: Option<PanelHit>) -> Pixmap {
    let mut pm = Pixmap::new(l.width.max(1), l.height.max(1)).expect("non-zero size");
    let (w, h) = (l.width as f32, l.height as f32);
    fill_rounded_rect(&mut pm, 0.0, 0.0, w, h, st.radius, theme.surface);

    if l.empty {
        let msg = "No notifications";
        let tw = text.measure(msg, Role::UI, st.text_size).0;
        text.draw(&mut pm, msg, (w - tw) / 2.0, st.header_h as f32 + 20.0, Role::UI, st.text_size, theme.text_dim);
    }
    for r in &l.rows {
        let (y, x0) = (r.rect.y as f32, st.pad);
        if hover == Some(PanelHit::Row(r.id)) || hover == Some(PanelHit::Remove(r.id)) {
            fill_rect(&mut pm, 1.0, y, w - 2.0, st.row_h as f32, theme.surface_hi);
        }
        if r.unread {
            let dot = if r.urgency == Urgency::Critical { theme.err } else { theme.accent };
            fill_rounded_rect(&mut pm, x0, y + 21.0, 6.0, 6.0, 3.0, dot);
        }
        text.draw(&mut pm, &r.app, x0 + 14.0, y + 8.0, Role::UI, st.small_size, theme.text_dim);
        text.draw(&mut pm, &r.summary, x0 + 14.0, y + 25.0, Role::UI, st.text_size, theme.text);
        let aw = text.measure(&r.age, Role::UI, st.small_size).0;
        text.draw(&mut pm, &r.age, r.remove.x as f32 - 8.0 - aw, y + 8.0, Role::UI, st.small_size, theme.text_dim);
        let col = if hover == Some(PanelHit::Remove(r.id)) { theme.text } else { theme.text_dim };
        let xw = text.measure("\u{d7}", Role::UI, st.title_size).0;
        text.draw(&mut pm, "\u{d7}", r.remove.x as f32 + (18.0 - xw) / 2.0, r.remove.y as f32, Role::UI, st.title_size, col);
        fill_rect(&mut pm, x0, y + st.row_h as f32 - 1.0, w - 2.0 * x0, 1.0, theme.border);
    }

    // the header covers rows scrolled up under it
    fill_rounded_rect(&mut pm, 0.0, 0.0, w, st.header_h as f32, st.radius, theme.surface);
    fill_rect(&mut pm, 0.0, st.radius, w, st.header_h as f32 - st.radius, theme.surface);
    fill_rect(&mut pm, st.pad, st.header_h as f32 - 1.0, w - 2.0 * st.pad, 1.0, theme.border);
    text.draw(&mut pm, "Notifications", st.pad, (st.header_h as f32 - st.title_size * 1.2) / 2.0, Role::UI, st.title_size, theme.text);
    text.draw(&mut pm, "Notifications", st.pad + 0.6, (st.header_h as f32 - st.title_size * 1.2) / 2.0, Role::UI, st.title_size, theme.text);

    let pill = |pm: &mut Pixmap, text: &mut TextSystem, r: Rect, label: &str, size: f32, on: bool, hot: bool| {
        let (bg, fg) = if on || hot { (theme.accent, theme.accent_text) } else { (theme.surface_hi, theme.text) };
        fill_rounded_rect(pm, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, bg);
        let (tw, th) = text.measure(label, Role::UI, size);
        text.draw(pm, label, r.x as f32 + (r.w as f32 - tw) / 2.0, r.y as f32 + (r.h as f32 - th) / 2.0, Role::UI, size, fg);
    };
    pill(&mut pm, text, l.dnd, "Do not disturb", st.small_size, l.dnd_on, hover == Some(PanelHit::Dnd));
    pill(&mut pm, text, l.clear, "Clear", st.text_size, false, hover == Some(PanelHit::Clear));
    let col = if hover == Some(PanelHit::Close) { theme.text } else { theme.text_dim };
    let xw = text.measure("\u{d7}", Role::UI, st.title_size).0;
    text.draw(&mut pm, "\u{d7}", l.close.x as f32 + (18.0 - xw) / 2.0, l.close.y as f32, Role::UI, st.title_size, col);

    stroke_rounded_rect(&mut pm, 0.5, 0.5, w - 1.0, h - 1.0, st.radius, theme.border, 1.0);
    pm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: u32) -> Vec<Row> {
        (1..=n).map(|id| Row { id, app: "Mail".into(), summary: format!("message {id}"), created_ms: 0, unread: id % 2 == 0, urgency: Urgency::Normal }).collect()
    }

    fn lay(n: u32, scroll: i32) -> PanelLayout {
        layout(&rows(n), false, scroll, 0, &PanelStyle::default(), &mut TextSystem::bitmap_only())
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let p = pm.pixel(x, y).unwrap();
        [p.red(), p.green(), p.blue(), p.alpha()]
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(age_label(1000, 1000), "now");
        assert_eq!(age_label(0, 59_999), "now");
        assert_eq!(age_label(0, 60_000), "1m");
        assert_eq!(age_label(0, 3_599_000), "59m");
        assert_eq!(age_label(0, 3_600_000), "1h");
        assert_eq!(age_label(0, 90_000_000), "1d");
        assert_eq!(age_label(5000, 0), "now", "a clock that went backwards");
    }

    #[test]
    fn an_empty_panel_is_a_header_and_a_message() {
        let l = lay(0, 0);
        assert!(l.empty && l.rows.is_empty());
        assert_eq!(l.height, 44 + 64);
    }

    #[test]
    fn height_grows_with_rows_then_caps_and_scrolls() {
        assert_eq!(lay(3, 0).height, 44 + 3 * 54);
        let l = lay(30, 0);
        assert_eq!(l.height, 560, "capped");
        assert_eq!(l.max_scroll, 30 * 54 - (560 - 44));
        assert!(l.rows.len() < 30, "only rows in view are laid out");
        assert_eq!(lay(30, 100_000).scroll, l.max_scroll, "scroll is clamped");
        assert_eq!(lay(30, -50).scroll, 0);
        assert_eq!(lay(3, 99).scroll, 0, "nothing to scroll when everything fits");
    }

    #[test]
    fn scrolling_moves_rows_up() {
        let (a, b) = (lay(30, 0), lay(30, 54));
        assert_eq!((a.rows[0].id, a.rows[0].rect.y), (1, 44));
        // scrolled by one row: row 1 is entirely under the header and dropped
        assert_eq!((b.rows[0].id, b.rows[0].rect.y), (2, 44));
    }

    #[test]
    fn hit_testing() {
        let st = PanelStyle::default();
        let l = lay(3, 0);
        assert_eq!(hit(&l, l.close.x + 2, l.close.y + 2, &st), Some(PanelHit::Close));
        assert_eq!(hit(&l, l.clear.x + 2, l.clear.y + 2, &st), Some(PanelHit::Clear));
        assert_eq!(hit(&l, l.dnd.x + 2, l.dnd.y + 2, &st), Some(PanelHit::Dnd));
        assert_eq!(hit(&l, 100, 10, &st), None, "empty header space");
        let r = &l.rows[1];
        assert_eq!(hit(&l, r.remove.x + 2, r.remove.y + 2, &st), Some(PanelHit::Remove(2)));
        assert_eq!(hit(&l, 20, r.rect.y + 5, &st), Some(PanelHit::Row(2)));
        assert_eq!(hit(&l, -1, 5, &st), None);
        assert_eq!(hit(&l, 5, l.height as i32, &st), None);
    }

    #[test]
    fn rows_never_cover_the_header_or_leave_the_panel() {
        for scroll in [0, 7, 54, 300, 1000] {
            let l = lay(40, scroll);
            for r in &l.rows {
                assert!(r.rect.y + r.rect.h as i32 > 44 && r.rect.y < l.height as i32);
            }
        }
    }

    #[test]
    fn draw_has_the_right_size_a_rounded_corner_and_an_unread_dot() {
        let (st, theme) = (PanelStyle::default(), Theme::default());
        let mut t = TextSystem::bitmap_only();
        let l = lay(3, 0);
        let pm = draw(&l, &st, &theme, &mut t, None);
        assert_eq!((pm.width(), pm.height()), (l.width, l.height));
        assert_eq!(px(&pm, 0, 0)[3], 0, "rounded corner");
        let rgb = |c: tiny_skia::Color| {
            let c = c.to_color_u8();
            [c.red(), c.green(), c.blue()]
        };
        let r = &l.rows[1]; // id 2 is unread
        assert_eq!(&px(&pm, 15, (r.rect.y + 24) as u32)[..3], &rgb(theme.accent)[..], "unread dot");
        let read = &l.rows[0]; // id 1 is read: no dot
        assert_ne!(&px(&pm, 15, (read.rect.y + 24) as u32)[..3], &rgb(theme.accent)[..]);
    }

    #[test]
    fn draw_survives_hostile_input() {
        let (st, theme) = (PanelStyle::default(), Theme::default());
        let mut t = TextSystem::bitmap_only();
        let hostile = Row { id: 1, app: "A".repeat(500), summary: "\u{202e}\n<b>x".repeat(100), created_ms: u64::MAX, unread: true, urgency: Urgency::Critical };
        let l = layout(&vec![hostile; 50], true, 77, 0, &st, &mut t);
        let pm = draw(&l, &st, &theme, &mut t, Some(PanelHit::Remove(1)));
        assert_eq!(pm.height(), l.height);
    }
}
