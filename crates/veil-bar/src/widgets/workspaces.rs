//! Workspace indicator: numbered pills showing active and occupied workspaces.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{fill_rounded_rect, Action, Metrics, Theme, Widget};

/// Pill width and gap at the default font size.
const PILL_W: f32 = 26.0;
const PILL_GAP: f32 = 4.0;

pub struct WorkspacesWidget {
    pub active: usize,
    pub occupied: Vec<usize>,
    /// Pill pitch of the last draw, so a click maps back to a workspace.
    pitch: f32,
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
            pitch: PILL_W + PILL_GAP,
        }
    }

    /// Display up to the highest occupied or active workspace, at least 4.
    fn shown(&self) -> usize {
        self.occupied.iter().copied().max().unwrap_or(1).max(self.active).max(4)
    }

    /// Which workspace a click `x` px into the widget lands on.
    fn workspace_at(&self, x: f32) -> Option<usize> {
        if x < 0.0 || self.pitch <= 0.0 {
            return None;
        }
        let n = (x / self.pitch) as usize + 1;
        (n <= self.shown()).then_some(n)
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
        m: &Metrics,
        mut x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        let start_x = x;
        let pad_v = m.px(4.0);
        let pill_h = h - pad_v * 2.0;
        let pill_w = m.px(PILL_W);
        let gap = m.px(PILL_GAP);
        self.pitch = pill_w + gap;

        for num in 1..=self.shown() {
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

            fill_rounded_rect(pixmap, x, y + pad_v, pill_w, pill_h, m.px(4.0), bg);

            let label = num.to_string();
            let (lw, _) = text.measure(&label, Role::Mono, m.small);
            let tx = x + (pill_w - lw) / 2.0;
            let ty = y + (h - m.small) / 2.0;
            text.draw(pixmap, &label, tx, ty, Role::Mono, m.small, fg);

            x += pill_w + gap;
        }

        x - start_x
    }

    fn on_click(&mut self, x: f32) -> Option<Action> {
        let n = self.workspace_at(x)?;
        Some(Action::Ipc { cmd: "workspace.switch".into(), args: serde_json::json!({ "n": n }) })
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_maps_to_the_pill_under_it() {
        let w = WorkspacesWidget::new(); // pitch 30, 4 shown
        assert_eq!(w.workspace_at(0.0), Some(1));
        assert_eq!(w.workspace_at(29.0), Some(1));
        assert_eq!(w.workspace_at(30.0), Some(2));
        assert_eq!(w.workspace_at(119.0), Some(4));
        assert_eq!(w.workspace_at(120.0), None, "past the last pill");
        assert_eq!(w.workspace_at(-1.0), None);
    }

    #[test]
    fn click_sends_workspace_switch() {
        let mut w = WorkspacesWidget::new();
        assert_eq!(
            w.on_click(35.0),
            Some(Action::Ipc { cmd: "workspace.switch".into(), args: serde_json::json!({ "n": 2 }) })
        );
    }

    #[test]
    fn shows_more_pills_when_a_high_workspace_is_occupied() {
        let mut w = WorkspacesWidget::new();
        w.occupied = vec![1, 7];
        assert_eq!(w.workspace_at(6.0 * 30.0 + 1.0), Some(7));
    }
}
