//! Widget trait for bar and shell surfaces, plus the metrics every widget
//! scales from.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::{TextSystem, BASE_SIZE};
use crate::theme::Theme;

/// What a click on a widget asks the host application to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Run a command line (`sh -c`), detached.
    Exec(String),
    /// Send an IPC command to the compositor.
    Ipc { cmd: String, args: serde_json::Value },
}

/// Sizes every widget derives from `font.size` (section 7.1: "every size
/// scales from `font.size`"). At the default 14 px they are the numbers the
/// widgets used to hard-code.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// `font.size / 14`.
    pub scale: f32,
    /// Primary text size.
    pub font: f32,
    /// Secondary text size.
    pub small: f32,
    /// Padding inside a widget.
    pub pad: f32,
    /// Gap between widgets.
    pub gap: f32,
    /// Corner radius of pills and highlights.
    pub radius: f32,
}

impl Metrics {
    /// `font_size <= 0` means "not configured": the 14 px default.
    pub fn from_font_size(font_size: f32) -> Self {
        let size = if font_size > 0.0 { font_size.max(8.0) } else { BASE_SIZE };
        let s = size / BASE_SIZE;
        Self {
            scale: s,
            font: 13.0 * s,
            small: 12.0 * s,
            pad: 8.0 * s,
            gap: 8.0 * s,
            radius: 6.0 * s,
        }
    }

    /// A length given in px at the default size, scaled.
    pub fn px(&self, base: f32) -> f32 {
        base * self.scale
    }

    /// Bar height in physical-independent px for a configured height.
    pub fn bar_height(&self, configured: u32) -> u32 {
        (configured as f32 * self.scale).round().max(1.0) as u32
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::from_font_size(0.0)
    }
}

pub trait Widget: Send {
    fn name(&self) -> &str;
    fn update(&mut self, now: Instant) -> bool;
    /// Draw at `(x, y)` inside a surface `h` tall and return the width used.
    /// Called with a 1x1 scratch pixmap to measure, so it must not depend on
    /// the pixmap size.
    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        m: &Metrics,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32;
    /// A left click `x` px into the widget.
    fn on_click(&mut self, _x: f32) -> Option<Action> {
        None
    }
    /// Scroll over the widget: negative is up, positive is down.
    fn on_scroll(&mut self, _dy: f32) -> Option<Action> {
        None
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_metrics_are_the_old_hardcoded_numbers() {
        let m = Metrics::from_font_size(0.0);
        assert_eq!((m.font, m.small, m.pad, m.gap, m.radius), (13.0, 12.0, 8.0, 8.0, 6.0));
        assert_eq!(m, Metrics::from_font_size(14.0));
        assert_eq!(m.bar_height(36), 36);
    }

    #[test]
    fn everything_scales_with_font_size() {
        let m = Metrics::from_font_size(28.0);
        assert_eq!((m.scale, m.font, m.pad), (2.0, 26.0, 16.0));
        assert_eq!(m.bar_height(36), 72);
        assert_eq!(m.px(10.0), 20.0);
        // Absurdly small sizes are floored so nothing collapses.
        assert!(Metrics::from_font_size(1.0).font >= 7.0);
    }
}
