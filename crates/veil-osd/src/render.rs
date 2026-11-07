//! Drawing the OSD pill. Pure: a pixmap in, premultiplied RGBA bytes out.

use tiny_skia::Pixmap;
use veil_ipc::osd::{OsdEvent, OsdKind};
use veil_ui::{fill_rounded_rect, stroke_rounded_rect, Metrics, Role, TextSystem, Theme};

/// Surface geometry for one font size. Every size scales from `font.size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    /// Gap between the pill and the bottom edge of the usable area.
    pub margin_bottom: i32,
}

pub fn geometry(m: &Metrics) -> Geometry {
    Geometry {
        width: m.px(340.0).round() as u32,
        height: m.px(64.0).round() as u32,
        margin_bottom: m.px(72.0).round() as i32,
    }
}

/// Render the pill at `alpha` (0.0 to 1.0). The whole pixmap fades together,
/// so the border and the text never pop in separately.
pub fn render(ts: &mut TextSystem, theme: &Theme, m: &Metrics, ev: &OsdEvent, w: u32, h: u32, alpha: f32) -> Vec<u8> {
    let Some(mut pm) = Pixmap::new(w.max(1), h.max(1)) else {
        return Vec::new();
    };
    if alpha <= 0.0 {
        return pm.take();
    }
    let (wf, hf) = (w as f32, h as f32);
    let r = m.px(14.0);
    let pad = m.px(16.0);

    let border = if ev.kind == OsdKind::Error { theme.err } else { theme.border };
    fill_rounded_rect(&mut pm, 0.0, 0.0, wf, hf, r, theme.surface);
    stroke_rounded_rect(&mut pm, 0.5, 0.5, wf - 1.0, hf - 1.0, r, border, 1.0);

    match ev.progress {
        Some(p) => draw_level(ts, theme, m, ev, p, &mut pm, wf, hf, pad),
        None => draw_message(ts, theme, m, ev, &mut pm, wf, hf, pad),
    }

    let mut bytes = pm.take();
    if alpha < 1.0 {
        let a = (alpha.clamp(0.0, 1.0) * 256.0) as u32;
        for b in &mut bytes {
            *b = ((u32::from(*b) * a) >> 8) as u8;
        }
    }
    bytes
}

#[allow(clippy::too_many_arguments)]
fn draw_level(ts: &mut TextSystem, theme: &Theme, m: &Metrics, ev: &OsdEvent, p: u8, pm: &mut Pixmap, w: f32, h: f32, pad: f32) {
    let pct = format!("{p}%");
    let (pct_w, _) = ts.measure(&pct, Role::UI, m.font);
    let title_max = (w - pad * 2.0 - pct_w - m.gap).max(0.0);
    let title = ts.ellipsize(&ev.title, Role::UI, m.font, title_max);
    let top = m.px(12.0);
    ts.draw(pm, &title, pad, top, Role::UI, m.font, theme.text);
    ts.draw(pm, &pct, w - pad - pct_w, top, Role::UI, m.font, theme.text_dim);

    let bar_h = m.px(6.0);
    let bar_y = h - m.px(18.0) - bar_h / 2.0;
    let bar_w = w - pad * 2.0;
    fill_rounded_rect(pm, pad, bar_y, bar_w, bar_h, bar_h / 2.0, theme.surface_hi);
    let fill = bar_w * f32::from(p.min(100)) / 100.0;
    if fill >= 1.0 {
        let color = if ev.kind == OsdKind::Error { theme.err } else { theme.accent };
        fill_rounded_rect(pm, pad, bar_y, fill, bar_h, bar_h / 2.0, color);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_message(ts: &mut TextSystem, theme: &Theme, m: &Metrics, ev: &OsdEvent, pm: &mut Pixmap, w: f32, h: f32, pad: f32) {
    let max = (w - pad * 2.0).max(0.0);
    let title_color = if ev.kind == OsdKind::Error { theme.err } else { theme.text };
    let title = ts.ellipsize(&ev.title, Role::UI, m.font, max);
    if ev.body.is_empty() {
        ts.draw(pm, &title, pad, (h - m.font) / 2.0, Role::UI, m.font, title_color);
        return;
    }
    ts.draw(pm, &title, pad, m.px(12.0), Role::UI, m.font, title_color);
    let body = ts.ellipsize(&ev.body, Role::UI, m.small, max);
    ts.draw(pm, &body, pad, m.px(34.0), Role::UI, m.small, theme.text_dim);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (TextSystem, Theme, Metrics) {
        (TextSystem::bitmap_only(), Theme::default(), Metrics::default())
    }

    fn px(bytes: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
    }

    #[test]
    fn geometry_scales_with_font_size() {
        let g14 = geometry(&Metrics::from_font_size(14.0));
        let g28 = geometry(&Metrics::from_font_size(28.0));
        assert_eq!((g14.width, g14.height), (340, 64));
        assert_eq!((g28.width, g28.height), (680, 128));
    }

    #[test]
    fn buffer_matches_the_surface_and_is_fully_transparent_at_alpha_zero() {
        let (mut ts, th, m) = setup();
        let ev = OsdEvent::new("Volume", "", Some(50), 2000, "");
        let out = render(&mut ts, &th, &m, &ev, 340, 64, 0.0);
        assert_eq!(out.len(), 340 * 64 * 4);
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn corners_are_transparent_and_the_middle_is_not() {
        let (mut ts, th, m) = setup();
        let ev = OsdEvent::new("Reloaded", "", None, 2000, "");
        let out = render(&mut ts, &th, &m, &ev, 340, 64, 1.0);
        assert_eq!(px(&out, 340, 0, 0)[3], 0);
        assert_eq!(px(&out, 340, 170, 4)[3], 255);
    }

    #[test]
    fn a_full_bar_is_accent_and_an_empty_one_is_track() {
        let (mut ts, th, m) = setup();
        let at = |p: u8, ts: &mut TextSystem| {
            let ev = OsdEvent::new("Volume", "", Some(p), 2000, "");
            let out = render(ts, &th, &m, &ev, 340, 64, 1.0);
            px(&out, 340, 300, 64 - 18)
        };
        let full = at(100, &mut ts);
        let empty = at(0, &mut ts);
        assert_ne!(full, empty);
        let acc = th.accent.to_color_u8();
        assert_eq!(&full[..3], &[acc.red(), acc.green(), acc.blue()]);
    }

    #[test]
    fn half_alpha_halves_the_coverage() {
        let (mut ts, th, m) = setup();
        let ev = OsdEvent::new("Reloaded", "", None, 2000, "");
        let full = render(&mut ts, &th, &m, &ev, 340, 64, 1.0);
        let half = render(&mut ts, &th, &m, &ev, 340, 64, 0.5);
        let (f, h) = (px(&full, 340, 170, 4)[3], px(&half, 340, 170, 4)[3]);
        assert!((i32::from(h) - i32::from(f) / 2).abs() <= 2, "{f} vs {h}");
    }

    #[test]
    fn long_text_and_tiny_surfaces_do_not_panic() {
        let (mut ts, th, m) = setup();
        let ev = OsdEvent::new(&"w".repeat(300), &"x".repeat(300), None, 2000, "");
        let _ = render(&mut ts, &th, &m, &ev, 340, 64, 1.0);
        let _ = render(&mut ts, &th, &m, &ev, 1, 1, 1.0);
        let lv = OsdEvent::new(&"w".repeat(300), "", Some(100), 2000, "");
        let _ = render(&mut ts, &th, &m, &lv, 20, 20, 1.0);
    }
}
