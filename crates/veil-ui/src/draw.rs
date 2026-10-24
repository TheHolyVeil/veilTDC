//! Shared tiny-skia drawing primitives for Veil UI surfaces.

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

/// Parse a hex color string (#RRGGBB or #AARRGGBB) into a tiny-skia Color.
pub fn hex_color(s: &str) -> Color {
    let s = s.trim_start_matches('#');
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).unwrap_or(0);
            let g = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
            let b = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
            Color::from_rgba8(r, g, b, 255)
        }
        8 => {
            let a = u8::from_str_radix(&s[0..2], 16).unwrap_or(255);
            let r = u8::from_str_radix(&s[2..4], 16).unwrap_or(0);
            let g = u8::from_str_radix(&s[4..6], 16).unwrap_or(0);
            let b = u8::from_str_radix(&s[6..8], 16).unwrap_or(0);
            Color::from_rgba8(r, g, b, a)
        }
        _ => Color::BLACK,
    }
}

/// Fill the entire pixmap with a solid color.
pub fn clear(pixmap: &mut Pixmap, color: Color) {
    pixmap.fill(color);
}

/// Fill a solid rectangle.
pub fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, color: Color) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let Some(rect) = Rect::from_xywh(x, y, w, h) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = false;
    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
}

/// Fill a rounded rectangle.
pub fn fill_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    color: Color,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    if r <= 0.0 || w < r * 2.0 || h < r * 2.0 {
        fill_rect(pixmap, x, y, w, h, color);
        return;
    }
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    if let Some(path) = rounded_rect_path(x, y, w, h, r) {
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Stroke a rounded rectangle outline.
pub fn stroke_rounded_rect(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    color: Color,
    width: f32,
) {
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = width;
    stroke.line_cap = LineCap::Round;
    stroke.line_join = LineJoin::Round;
    if let Some(path) = rounded_rect_path(x, y, w, h, r) {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Draw a filled circle.
pub fn fill_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, color: Color) {
    if r <= 0.0 {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    if let Some(path) = pb.finish() {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Construct a rounded rectangle Path.
pub fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_color_parsing() {
        let c = hex_color("#ff0000");
        assert_eq!(c, Color::from_rgba8(255, 0, 0, 255));
        let c_alpha = hex_color("#8000ff00");
        assert_eq!(c_alpha, Color::from_rgba8(0, 255, 0, 128));
    }

    #[test]
    fn draw_primitives_to_pixmap() {
        let mut pixmap = Pixmap::new(100, 50).unwrap();
        clear(&mut pixmap, Color::BLACK);
        fill_rect(&mut pixmap, 0.0, 0.0, 10.0, 10.0, Color::WHITE);
        fill_rounded_rect(&mut pixmap, 20.0, 0.0, 20.0, 20.0, 4.0, Color::WHITE);
        fill_circle(&mut pixmap, 60.0, 10.0, 5.0, Color::WHITE);

        let non_black = pixmap.pixels().iter().any(|p| p.red() > 0);
        assert!(non_black);
    }
}
