//! Activities button: opens the launcher.
//!
//! woven-bar's version ran an external overlay command; here the compositor
//! owns the launcher, so it is one IPC `component.toggle`.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Action, Metrics, Theme, Widget};

use crate::util::button;

pub struct ActivitiesWidget;

impl ActivitiesWidget {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ActivitiesWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for ActivitiesWidget {
    fn name(&self) -> &str {
        "activities"
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
        // nf-fa-th, present in every Nerd Font.
        button(pixmap, text, theme, m, x, y, h, "\u{f00b}", theme.accent, true)
    }

    fn on_click(&mut self, _x: f32) -> Option<Action> {
        Some(Action::Ipc { cmd: "component.toggle".into(), args: serde_json::json!({ "name": "launch" }) })
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_toggles_the_launcher() {
        let mut w = ActivitiesWidget::new();
        assert_eq!(
            w.on_click(3.0),
            Some(Action::Ipc { cmd: "component.toggle".into(), args: serde_json::json!({ "name": "launch" }) })
        );
    }
}
