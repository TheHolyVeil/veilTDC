//! Widget trait for bar and shell surfaces.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use crate::theme::Theme;

pub trait Widget: Send {
    fn name(&self) -> &str;
    fn update(&mut self, now: Instant) -> bool;
    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32;
    fn on_click(&mut self, _x: f32, _y: f32) -> Option<String> {
        None
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}
