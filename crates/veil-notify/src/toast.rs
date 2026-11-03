//! One toast: measure it, draw it, and say what a click on it means.
//!
//! Each toast is its own layer surface, so everything here is in that surface's
//! local coordinates and there is nothing to share with its neighbours.
//!
//! Drawn: an accent stripe by urgency, the app name, a bold summary, the body
//! with `<b>` (faux bold) and `<u>`, an optional image, action buttons and a
//! close button. Not drawn: `<i>` (the font stack has no italic, so it shows
//! upright) and `image-path` / `app_icon` files (step 5 handles `image-data`).

use tiny_skia::{Color, Pixmap};
use veil_ui::{fill_rect, fill_rounded_rect, stroke_rounded_rect, Role, TextSystem, Theme};

use crate::layout::Rect;
use crate::markup::{self, Span};
use crate::model::{Image, Notification, Urgency};
use crate::wrap::{self, Line};

/// Faux bold is the same glyphs drawn twice this far apart.
const BOLD_OFFSET: f32 = 0.6;

#[derive(Clone, Debug)]
pub struct ToastStyle {
    pub width: u32,
    pub pad: f32,
    pub radius: f32,
    pub stripe: f32,
    pub app_size: f32,
    pub summary_size: f32,
    pub body_size: f32,
    pub max_summary_lines: usize,
    pub max_body_lines: usize,
    pub image: u32,
    pub button_h: f32,
    pub button_gap: f32,
    pub close: u32,
    pub min_height: u32,
}

impl Default for ToastStyle {
    fn default() -> Self {
        ToastStyle {
            width: 360,
            pad: 12.0,
            radius: 10.0,
            stripe: 3.0,
            app_size: 11.0,
            summary_size: 14.0,
            body_size: 13.0,
            max_summary_lines: 2,
            max_body_lines: 5,
            image: 48,
            button_h: 26.0,
            button_gap: 6.0,
            close: 16,
            min_height: 48,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToastButton {
    pub key: String,
    pub label: String,
    pub rect: Rect,
}

#[derive(Clone, Debug)]
pub struct ToastLayout {
    pub width: u32,
    pub height: u32,
    app: String,
    app_y: f32,
    text_x: f32,
    summary: Vec<Line>,
    summary_y: f32,
    body: Vec<Line>,
    body_y: f32,
    line_h: (f32, f32, f32),
    pub image: Option<Rect>,
    pub buttons: Vec<ToastButton>,
    pub close: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastHit {
    Close,
    /// Index into [`ToastLayout::buttons`].
    Button(usize),
    /// Anywhere else on the toast: runs the default action, then dismisses.
    Body,
}

fn line_height(text: &mut TextSystem, size: f32) -> f32 {
    text.measure("A", Role::UI, size).1.ceil()
}

fn wrapped(text: &mut TextSystem, spans: &[Span], width: f32, size: f32, max_lines: usize) -> Vec<Line> {
    let mut measure = |t: &str, bold: bool| text.measure(t, Role::UI, size).0 + if bold { BOLD_OFFSET } else { 0.0 };
    wrap::wrap_spans(spans, width, max_lines, &mut measure)
}

/// Measure `n`: the surface size and where everything goes.
pub fn layout(n: &Notification, st: &ToastStyle, text: &mut TextSystem) -> ToastLayout {
    let w = st.width as f32;
    let left = st.pad + st.stripe;
    let text_x = if n.image.is_some() { left + st.image as f32 + 10.0 } else { left };
    let text_w = (w - text_x - st.pad).max(20.0);
    let close_w = st.close as f32 + 4.0;

    let (h_app, h_sum, h_body) =
        (line_height(text, st.app_size), line_height(text, st.summary_size), line_height(text, st.body_size));

    let mut y = st.pad;
    let app = text.ellipsize(&n.app_name, Role::UI, st.app_size, text_w - close_w);
    let app_y = y;
    if !app.is_empty() {
        y += h_app + 2.0;
    }

    let summary_span = [Span { text: n.summary.clone(), bold: true, italic: false, underline: false }];
    let summary = wrapped(text, &summary_span, text_w - close_w, st.summary_size, st.max_summary_lines);
    let summary_y = y;
    y += summary.len() as f32 * h_sum;

    let body = wrapped(text, &markup::parse(&n.body), text_w, st.body_size, st.max_body_lines);
    let mut body_y = y;
    if !body.is_empty() {
        y += 4.0;
        body_y = y;
        y += body.len() as f32 * h_body;
    }

    let image = n.image.as_ref().map(|_| Rect { x: left as i32, y: st.pad as i32, w: st.image, h: st.image });
    let mut bottom = y.max(image.map_or(0.0, |r| (r.y + r.h as i32) as f32));

    let mut buttons = Vec::new();
    let actions: Vec<_> = n.buttons().collect();
    if !actions.is_empty() {
        let by = bottom + 8.0;
        let total = w - left - st.pad;
        let count = actions.len() as f32;
        let bw = ((total - st.button_gap * (count - 1.0)) / count).floor().max(10.0);
        for (i, a) in actions.iter().enumerate() {
            let label = text.ellipsize(&a.label, Role::UI, st.body_size, bw - 12.0);
            let rect = Rect { x: (left + i as f32 * (bw + st.button_gap)) as i32, y: by as i32, w: bw as u32, h: st.button_h as u32 };
            buttons.push(ToastButton { key: a.key.clone(), label, rect });
        }
        bottom = by + st.button_h;
    }

    let height = ((bottom + st.pad).ceil() as u32).max(st.min_height);
    let close = Rect { x: (w - st.pad - st.close as f32) as i32, y: (st.pad - 2.0) as i32, w: st.close, h: st.close };
    ToastLayout { width: st.width, height, app, app_y, text_x, summary, summary_y, body, body_y, line_h: (h_app, h_sum, h_body), image, buttons, close }
}

/// What is under the surface-local point `(x, y)`; `None` outside the toast.
pub fn hit(l: &ToastLayout, x: i32, y: i32) -> Option<ToastHit> {
    if l.close.contains(x, y) {
        return Some(ToastHit::Close);
    }
    if let Some(i) = l.buttons.iter().position(|b| b.rect.contains(x, y)) {
        return Some(ToastHit::Button(i));
    }
    (x >= 0 && y >= 0 && x < l.width as i32 && y < l.height as i32).then_some(ToastHit::Body)
}

fn accent_for(n: &Notification, theme: &Theme) -> Color {
    match n.urgency {
        Urgency::Critical => theme.err,
        Urgency::Normal => theme.accent,
        Urgency::Low => theme.text_dim,
    }
}

fn draw_lines(pm: &mut Pixmap, text: &mut TextSystem, lines: &[Line], x: f32, y: f32, size: f32, line_h: f32, color: Color) {
    for (i, line) in lines.iter().enumerate() {
        let ly = y + i as f32 * line_h;
        let mut cx = x;
        for run in line {
            let mut adv = text.draw(pm, &run.text, cx, ly, Role::UI, size, color);
            if run.style.bold {
                text.draw(pm, &run.text, cx + BOLD_OFFSET, ly, Role::UI, size, color);
                adv += BOLD_OFFSET;
            }
            if run.style.underline {
                fill_rect(pm, cx, ly + line_h - 2.0, adv, 1.0, color);
            }
            cx += adv;
        }
    }
}

/// Source-over a straight-alpha image into `dst`, scaled to fit and centred
/// (nearest neighbour; images arrive already capped at 128 px).
fn draw_image(pm: &mut Pixmap, img: &Image, dst: Rect) {
    let (iw, ih) = (img.width as f32, img.height as f32);
    if iw == 0.0 || ih == 0.0 {
        return;
    }
    let scale = (dst.w as f32 / iw).min(dst.h as f32 / ih);
    let (ow, oh) = ((iw * scale).max(1.0) as i32, (ih * scale).max(1.0) as i32);
    let (ox, oy) = (dst.x + (dst.w as i32 - ow) / 2, dst.y + (dst.h as i32 - oh) / 2);
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for dy in 0..oh {
        for dx in 0..ow {
            let (px, py) = (ox + dx, oy + dy);
            if px < 0 || py < 0 || px >= pw || py >= ph {
                continue;
            }
            let (sx, sy) = ((dx as f32 / scale) as usize, (dy as f32 / scale) as usize);
            let s = (sy.min(img.height as usize - 1) * img.width as usize + sx.min(img.width as usize - 1)) * 4;
            let a = img.rgba[s + 3] as u32;
            let d = (py as usize * pw as usize + px as usize) * 4;
            for c in 0..3 {
                let premul = img.rgba[s + c] as u32 * a / 255;
                data[d + c] = (premul + data[d + c] as u32 * (255 - a) / 255).min(255) as u8;
            }
            data[d + 3] = (a + data[d + 3] as u32 * (255 - a) / 255).min(255) as u8;
        }
    }
}

/// Paint the toast. `hover` highlights the button or close box under the pointer.
pub fn draw(n: &Notification, l: &ToastLayout, st: &ToastStyle, theme: &Theme, text: &mut TextSystem, hover: Option<ToastHit>) -> Pixmap {
    let mut pm = Pixmap::new(l.width.max(1), l.height.max(1)).expect("non-zero size");
    let (w, h) = (l.width as f32, l.height as f32);
    fill_rounded_rect(&mut pm, 0.0, 0.0, w, h, st.radius, theme.surface);
    stroke_rounded_rect(&mut pm, 0.5, 0.5, w - 1.0, h - 1.0, st.radius, theme.border, 1.0);
    fill_rounded_rect(&mut pm, 1.0, st.radius, st.stripe, (h - 2.0 * st.radius).max(1.0), 1.5, accent_for(n, theme));

    if !l.app.is_empty() {
        text.draw(&mut pm, &l.app, l.text_x, l.app_y, Role::UI, st.app_size, theme.text_dim);
    }
    draw_lines(&mut pm, text, &l.summary, l.text_x, l.summary_y, st.summary_size, l.line_h.1, theme.text);
    draw_lines(&mut pm, text, &l.body, l.text_x, l.body_y, st.body_size, l.line_h.2, theme.text);
    if let (Some(img), Some(r)) = (&n.image, l.image) {
        draw_image(&mut pm, img, r);
    }

    for (i, b) in l.buttons.iter().enumerate() {
        let hot = hover == Some(ToastHit::Button(i));
        let (bg, fg) = if hot { (theme.accent, theme.accent_text) } else { (theme.surface_hi, theme.text) };
        let r = b.rect;
        fill_rounded_rect(&mut pm, r.x as f32, r.y as f32, r.w as f32, r.h as f32, 6.0, bg);
        let tw = text.measure(&b.label, Role::UI, st.body_size);
        text.draw(&mut pm, &b.label, r.x as f32 + (r.w as f32 - tw.0) / 2.0, r.y as f32 + (r.h as f32 - tw.1) / 2.0, Role::UI, st.body_size, fg);
    }

    let c = l.close;
    let color = if hover == Some(ToastHit::Close) { theme.text } else { theme.text_dim };
    let tw = text.measure("\u{d7}", Role::UI, st.summary_size);
    text.draw(&mut pm, "\u{d7}", c.x as f32 + (c.w as f32 - tw.0) / 2.0, c.y as f32 + (c.h as f32 - tw.1) / 2.0, Role::UI, st.summary_size, color);
    pm
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Hints, Limits, NotifyArgs, RawImage};

    fn text() -> TextSystem {
        TextSystem::bitmap_only()
    }

    fn note(summary: &str, body: &str, actions: &[&str], urgency: Option<u8>, image: bool) -> Notification {
        let image_data = image.then(|| RawImage { width: 2, height: 2, rowstride: 6, has_alpha: false, bits_per_sample: 8, channels: 3, data: vec![255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0] });
        Notification::from_args(
            1,
            NotifyArgs {
                app_name: "Mail".into(),
                summary: summary.into(),
                body: body.into(),
                actions: actions.iter().map(|s| s.to_string()).collect(),
                hints: Hints { urgency, image_data, ..Hints::default() },
                ..NotifyArgs::default()
            },
            &Limits::default(),
            0,
        )
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let p = pm.pixel(x, y).unwrap();
        [p.red(), p.green(), p.blue(), p.alpha()]
    }

    fn rgba8(c: Color) -> [u8; 3] {
        let c = c.to_color_u8();
        [c.red(), c.green(), c.blue()]
    }

    #[test]
    fn a_plain_toast_has_the_configured_width_and_a_sensible_height() {
        let st = ToastStyle::default();
        let l = layout(&note("Hello", "world", &[], None, false), &st, &mut text());
        assert_eq!(l.width, 360);
        assert!(l.height >= st.min_height && l.height < 200, "height {}", l.height);
        assert!(l.buttons.is_empty() && l.image.is_none());
    }

    #[test]
    fn long_text_is_capped_with_an_ellipsis() {
        let st = ToastStyle::default();
        let body = "word ".repeat(400);
        let l = layout(&note(&"S".repeat(300), &body, &[], None, false), &st, &mut text());
        assert!(l.summary.len() <= st.max_summary_lines && l.body.len() <= st.max_body_lines);
        let last: String = l.body.last().unwrap().iter().map(|r| r.text.as_str()).collect();
        assert!(last.ends_with('\u{2026}'), "{last:?}");
    }

    #[test]
    fn buttons_share_the_row_inside_the_toast() {
        let st = ToastStyle::default();
        let l = layout(&note("x", "y", &["a", "Reply", "b", "Archive", "c", "Mute"], None, false), &st, &mut text());
        assert_eq!(l.buttons.len(), 3);
        let r: Vec<_> = l.buttons.iter().map(|b| b.rect).collect();
        assert!(r.iter().all(|r| r.w == r[0].w && r.y == r[0].y), "equal widths on one row");
        for pair in r.windows(2) {
            assert!(pair[0].x + pair[0].w as i32 <= pair[1].x, "overlap");
        }
        let last = r.last().unwrap();
        assert!(last.x + last.w as i32 <= l.width as i32 - st.pad as i32 + 1, "pokes out of the toast");
        assert!(last.y + last.h as i32 <= l.height as i32, "below the bottom edge");
        assert_eq!(l.buttons[0].key, "a");
    }

    #[test]
    fn an_image_pushes_the_text_right_and_the_toast_taller_if_needed() {
        let st = ToastStyle::default();
        let (with, without) = (layout(&note("s", "", &[], None, true), &st, &mut text()), layout(&note("s", "", &[], None, false), &st, &mut text()));
        assert!(with.text_x > without.text_x + st.image as f32);
        let r = with.image.unwrap();
        assert_eq!((r.w, r.h), (48, 48));
        assert!(with.height >= (r.y + r.h as i32) as u32, "image must fit in the toast");
    }

    #[test]
    fn hit_testing_is_close_then_buttons_then_body_then_nothing() {
        let st = ToastStyle::default();
        let l = layout(&note("s", "b", &["a", "A", "b", "B"], None, false), &st, &mut text());
        let c = l.close;
        assert_eq!(hit(&l, c.x + 2, c.y + 2), Some(ToastHit::Close));
        let b1 = l.buttons[1].rect;
        assert_eq!(hit(&l, b1.x + 2, b1.y + 2), Some(ToastHit::Button(1)));
        assert_eq!(hit(&l, 5, 5), Some(ToastHit::Body));
        assert_eq!(hit(&l, -1, 5), None);
        assert_eq!(hit(&l, 5, l.height as i32), None);
        assert_eq!(hit(&l, l.width as i32, 5), None);
    }

    #[test]
    fn draw_makes_a_rounded_opaque_toast_with_an_urgency_stripe() {
        let (st, theme) = (ToastStyle::default(), Theme::default());
        let mut t = text();
        for (urgency, want) in [(None, theme.accent), (Some(2), theme.err), (Some(0), theme.text_dim)] {
            let n = note("Hello", "world", &[], urgency, false);
            let l = layout(&n, &st, &mut t);
            let pm = draw(&n, &l, &st, &theme, &mut t, None);
            assert_eq!((pm.width(), pm.height()), (l.width, l.height));
            assert_eq!(px(&pm, 0, 0)[3], 0, "the corner is rounded off");
            assert_eq!(px(&pm, l.width - 8, l.height / 2)[3], 255, "the body is opaque");
            let stripe = px(&pm, 2, l.height / 2);
            assert_eq!(&stripe[..3], &rgba8(want)[..], "stripe colour for urgency {urgency:?}");
        }
    }

    #[test]
    fn draw_puts_ink_where_the_text_is_and_not_elsewhere() {
        let (st, theme) = (ToastStyle::default(), Theme::default());
        let mut t = text();
        let n = note("Hello", "", &[], None, false);
        let l = layout(&n, &st, &mut t);
        let pm = draw(&n, &l, &st, &theme, &mut t, None);
        let bg = rgba8(theme.surface);
        let inked = |x0: u32, y0: u32, x1: u32, y1: u32| (y0..y1).any(|y| (x0..x1).any(|x| px(&pm, x, y)[..3] != bg[..]));
        assert!(inked(l.text_x as u32, l.summary_y as u32, l.text_x as u32 + 60, l.summary_y as u32 + 14), "no summary ink");
        assert!(!inked(l.text_x as u32 + 120, l.summary_y as u32 + 20, l.width - 40, l.height - 6), "ink in an empty area");
    }

    #[test]
    fn draw_shows_the_image_and_highlights_a_hovered_button() {
        let (st, theme) = (ToastStyle::default(), Theme::default());
        let mut t = text();
        let n = note("s", "b", &["go", "Go"], None, true);
        let l = layout(&n, &st, &mut t);
        let plain = draw(&n, &l, &st, &theme, &mut t, None);
        let r = l.image.unwrap();
        assert_eq!(px(&plain, (r.x + 24) as u32, (r.y + 24) as u32), [255, 0, 0, 255], "the red test image");
        let b = l.buttons[0].rect;
        let (bx, by) = ((b.x + 3) as u32, (b.y + 3) as u32);
        assert_eq!(&px(&plain, bx, by)[..3], &rgba8(theme.surface_hi)[..]);
        let hot = draw(&n, &l, &st, &theme, &mut t, Some(ToastHit::Button(0)));
        assert_eq!(&px(&hot, bx, by)[..3], &rgba8(theme.accent)[..], "hover uses the accent");
    }

    #[test]
    fn layout_and_draw_survive_hostile_input() {
        let (st, theme) = (ToastStyle::default(), Theme::default());
        let mut t = text();
        let mut seed = 0xDEAD_BEEF_CAFE_F00Du64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let alphabet: Vec<char> = "ab <b>i</b><u>x</u>\n\u{202E}&;\u{e9}W".chars().collect();
        for _ in 0..200 {
            let mut s = |len: u64| -> String { (0..next(len)).map(|_| alphabet[next(alphabet.len() as u64) as usize]).collect() };
            let (summary, body) = (s(120), s(600));
            let n = note(&summary, &body, &["a", &s(40), "b", &s(10)], Some(next(4) as u8), next(2) == 0);
            let l = layout(&n, &st, &mut t);
            assert!(l.height >= st.min_height);
            for b in &l.buttons {
                assert!(b.rect.x >= 0 && b.rect.x + b.rect.w as i32 <= l.width as i32 && b.rect.y + b.rect.h as i32 <= l.height as i32);
            }
            let pm = draw(&n, &l, &st, &theme, &mut t, Some(ToastHit::Body));
            assert_eq!((pm.width(), pm.height()), (l.width, l.height));
        }
    }
}
