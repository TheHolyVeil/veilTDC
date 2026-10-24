//! Clock and calendar widget with formatted date and time.

use chrono::Local;
use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{Theme, Widget};

pub struct ClockWidget {
    format: String,
    last_rendered: String,
}

impl Default for ClockWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockWidget {
    pub fn new() -> Self {
        Self {
            format: "%a %b %d  %H:%M".into(),
            last_rendered: String::new(),
        }
    }
}

impl Widget for ClockWidget {
    fn name(&self) -> &str {
        "clock"
    }

    fn update(&mut self, _now: Instant) -> bool {
        let current = Local::now().format(&self.format).to_string();
        if current != self.last_rendered {
            self.last_rendered = current;
            true
        } else {
            false
        }
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        let current = Local::now().format(&self.format).to_string();
        let ty = y + (h - 13.0) / 2.0;
        let drawn = text.draw(pixmap, &current, x + 8.0, ty, Role::Mono, 13.0, theme.text);
        drawn + 16.0
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
