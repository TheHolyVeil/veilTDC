//! Bar layout and multi-section widget rendering.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_ui::{clear, fill_rect, Theme, Widget};
use veil_text::{FontConfig, TextSystem};

use crate::widgets::{
    AudioWidget, BatteryWidget, ClockWidget, CpuWidget, MemoryWidget, WindowTitleWidget,
    WorkspacesWidget,
};

pub struct BarLayout {
    pub left: Vec<Box<dyn Widget>>,
    pub center: Vec<Box<dyn Widget>>,
    pub right: Vec<Box<dyn Widget>>,
    pub theme: Theme,
    pub text: TextSystem,
    pub height: u32,
}

impl Default for BarLayout {
    fn default() -> Self {
        Self::new(36)
    }
}

impl BarLayout {
    pub fn new(height: u32) -> Self {
        let left: Vec<Box<dyn Widget>> = vec![
            Box::new(WorkspacesWidget::new()),
            Box::new(WindowTitleWidget::new()),
        ];
        let center: Vec<Box<dyn Widget>> = vec![
            Box::new(ClockWidget::new()),
        ];
        let right: Vec<Box<dyn Widget>> = vec![
            Box::new(CpuWidget::new()),
            Box::new(MemoryWidget::new()),
            Box::new(BatteryWidget::new()),
            Box::new(AudioWidget::new()),
        ];

        Self {
            left,
            center,
            right,
            theme: Theme::default(),
            text: TextSystem::new(&FontConfig::default()),
            height,
        }
    }

    /// Update dynamic widgets on tick. Returns true if any widget needs redraw.
    pub fn update(&mut self, now: Instant) -> bool {
        let mut needs_redraw = false;
        for w in self.left.iter_mut().chain(self.center.iter_mut()).chain(self.right.iter_mut()) {
            if w.update(now) {
                needs_redraw = true;
            }
        }
        needs_redraw
    }

    /// Handle IPC workspace event.
    pub fn on_workspaces(&mut self, active: usize, occupied: Vec<usize>) {
        for w in &mut self.left {
            if let Some(ws) = w.as_any_mut().downcast_mut::<WorkspacesWidget>() {
                ws.active = active;
                ws.occupied = occupied.clone();
            }
        }
    }

    /// Handle IPC focus / window title event.
    pub fn on_focus(&mut self, title: String) {
        for w in &mut self.left {
            if let Some(wt) = w.as_any_mut().downcast_mut::<WindowTitleWidget>() {
                wt.title = title.clone();
            }
        }
    }

    /// Handle IPC volume event.
    pub fn on_volume(&mut self, volume: u8, muted: bool) {
        for w in &mut self.right {
            if let Some(aw) = w.as_any_mut().downcast_mut::<AudioWidget>() {
                aw.volume = volume;
                aw.muted = muted;
            }
        }
    }

    /// Render entire bar into an ARGB8888 byte vector suitable for Wayland SHM commit.
    pub fn render(&mut self, width: u32, height: u32) -> Vec<u8> {
        let mut pixmap = match Pixmap::new(width, height) {
            Some(p) => p,
            None => return Vec::new(),
        };

        // Bar background
        clear(&mut pixmap, self.theme.bg);

        // Bottom border line
        fill_rect(&mut pixmap, 0.0, height as f32 - 1.0, width as f32, 1.0, self.theme.border);

        let h = height as f32;

        // Render Left section
        let mut left_x = 8.0f32;
        for w in &mut self.left {
            let advance = w.draw(&mut pixmap, &mut self.text, &self.theme, left_x, 0.0, h);
            left_x += advance + 8.0;
        }

        // Render Center section
        // Measure center items first to center on bar
        let mut center_w = 0.0f32;
        for w in &mut self.center {
            let mut dummy = Pixmap::new(1, 1).unwrap();
            let adv = w.draw(&mut dummy, &mut self.text, &self.theme, 0.0, 0.0, h);
            center_w += adv;
        }
        let mut center_x = ((width as f32 - center_w) / 2.0).max(left_x);
        for w in &mut self.center {
            let advance = w.draw(&mut pixmap, &mut self.text, &self.theme, center_x, 0.0, h);
            center_x += advance;
        }

        // Render Right section (accumulate backwards from right edge)
        let mut right_widths = Vec::new();
        for w in &mut self.right {
            let mut dummy = Pixmap::new(1, 1).unwrap();
            let adv = w.draw(&mut dummy, &mut self.text, &self.theme, 0.0, 0.0, h);
            right_widths.push(adv + 6.0);
        }

        let total_right_w: f32 = right_widths.iter().sum();
        let mut rx = (width as f32 - total_right_w - 8.0).max(center_x + 10.0);
        for w in &mut self.right {
            let adv = w.draw(&mut pixmap, &mut self.text, &self.theme, rx, 0.0, h);
            rx += adv + 6.0;
        }

        // Return raw RGBA/ARGB bytes
        pixmap.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_layout_render_produces_non_empty_buffer() {
        let mut bar = BarLayout::new(36);
        let pixels = bar.render(1920, 36);
        assert_eq!(pixels.len(), 1920 * 36 * 4);

        // Verify not completely empty
        let non_zero = pixels.iter().any(|&b| b > 0);
        assert!(non_zero);
    }
}
