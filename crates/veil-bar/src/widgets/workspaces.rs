//! Workspace indicator: numbered pills showing active and occupied workspaces.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{fill_rounded_rect, Theme, Widget};

pub struct WorkspacesWidget {
    pub active: usize,
    pub occupied: Vec<usize>,
}

impl Default for WorkspacesWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspacesWidget {
    pub fn new() -> Self {
        Self {
            active: 1,
            occupied: vec![1],
        }
    }
}

impl Widget for WorkspacesWidget {
    fn name(&self) -> &str {
        "workspaces"
    }

    fn update(&mut self, _now: Instant) -> bool {
        false
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &mut TextSystem,
        theme: &Theme,
        mut x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        let start_x = x;
        let pad_v = 4.0;
        let pill_h = h - pad_v * 2.0;
        let pill_w = 26.0;
        let gap = 4.0;
        let radius = 4.0;

        // Display up to 9 workspaces or highest occupied
        let max_ws = self.occupied.iter().copied().max().unwrap_or(1).max(self.active).max(4);

        for num in 1..=max_ws {
            let is_active = num == self.active;
            let is_occupied = self.occupied.contains(&num);

            let bg = if is_active {
                theme.accent
            } else if is_occupied {
                theme.surface_hi
            } else {
                theme.surface
            };

            let fg = if is_active {
                theme.accent_text
            } else if is_occupied {
                theme.text
            } else {
                theme.text_dim
            };

            fill_rounded_rect(pixmap, x, y + pad_v, pill_w, pill_h, radius, bg);

            let label = num.to_string();
            let (lw, _) = text.measure(&label, Role::Mono, 12.0);
            let tx = x + (pill_w - lw) / 2.0;
            let ty = y + (h - 12.0) / 2.0;
            text.draw(pixmap, &label, tx, ty, Role::Mono, 12.0, fg);

            x += pill_w + gap;
        }

        x - start_x
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
