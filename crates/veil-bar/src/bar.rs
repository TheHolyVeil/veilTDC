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
    /// Workspace state per monitor (index == compositor monitor index): each
    /// monitor has its own workspaces, so each bar draws its own.
    ws: Vec<(usize, Vec<usize>)>,
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
            ws: Vec::new(),
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

    /// Handle IPC workspace event: one (active, occupied) pair per monitor.
    pub fn on_workspaces(&mut self, monitors: Vec<(usize, Vec<usize>)>) {
        self.ws = monitors;
    }

    /// Monitor index behind a wl_output name. Veil names them `veil-host-<n>`;
    /// anything else (or no name) is monitor 0.
    fn monitor_of(output_name: &str) -> usize {
        output_name
            .strip_prefix("veil-host-")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
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

    /// Render the bar for the monitor behind `output_name`, using that
    /// monitor's own workspace state.
    pub fn render_for(&mut self, output_name: &str, width: u32, height: u32) -> Vec<u8> {
        if let Some((active, occupied)) = self.ws.get(Self::monitor_of(output_name)).cloned() {
            for w in &mut self.left {
                if let Some(ws) = w.as_any_mut().downcast_mut::<WorkspacesWidget>() {
                    ws.active = active;
                    ws.occupied = occupied.clone();
                }
            }
        }
        self.render(width, height)
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

#[cfg(test)]
mod monitor_tests {
    use super::BarLayout;

    #[test]
    fn monitor_index_comes_from_the_veil_output_name() {
        assert_eq!(BarLayout::monitor_of("veil-host-0"), 0);
        assert_eq!(BarLayout::monitor_of("veil-host-1"), 1);
        // Not ours (another compositor, or no name): monitor 0.
        assert_eq!(BarLayout::monitor_of("DP-2"), 0);
        assert_eq!(BarLayout::monitor_of(""), 0);
    }

    #[test]
    fn each_monitor_draws_its_own_workspace_state() {
        let mut bar = BarLayout::new(36);
        bar.on_workspaces(vec![(1, vec![1]), (3, vec![3, 4])]);
        let a = bar.render_for("veil-host-0", 400, 36);
        let b = bar.render_for("veil-host-1", 400, 36);
        assert!(!a.is_empty() && !b.is_empty());
        assert_ne!(a, b, "monitor 1 must not render monitor 0's workspaces");
    }
}
