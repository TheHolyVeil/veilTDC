//! Window title widget showing focused window title with automatic ellipsizing.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{Metrics, Theme, Widget};

pub struct WindowTitleWidget {
    pub title: String,
    /// Widest the title may get at the default font size; scales with it.
    pub max_width: f32,
}

impl Default for WindowTitleWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowTitleWidget {
    pub fn new() -> Self {
        Self {
            title: String::new(),
            max_width: 350.0,
        }
    }
}

impl Widget for WindowTitleWidget {
    fn name(&self) -> &str {
        "window_title"
    }

    fn update(&mut self, _now: Instant) -> bool {
        false
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        m: &Metrics,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        if self.title.is_empty() {
            return 0.0;
        }

        let ellipsized = text.ellipsize(&self.title, Role::UI, m.font, m.px(self.max_width));
        let ty = y + (h - m.font) / 2.0;
        let drawn = text.draw(pixmap, &ellipsized, x + m.pad, ty, Role::UI, m.font, theme.text);
        drawn + m.pad * 2.0
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
