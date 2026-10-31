//! Control center button: opens the settings quick panel.
//!
//! The panel is `veil-settings` (phase 7). Until it exists the compositor
//! answers the toggle with an error, which is logged and otherwise ignored.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Action, Metrics, Theme, Widget};

use crate::util::button;

pub struct ControlCenterWidget;

impl ControlCenterWidget {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ControlCenterWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for ControlCenterWidget {
    fn name(&self) -> &str {
        "control_center"
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
        // nf-md-cog
        button(pixmap, text, theme, m, x, y, h, "\u{f0493}", theme.text, true)
    }

    fn on_click(&mut self, _x: f32) -> Option<Action> {
        Some(Action::Ipc { cmd: "component.toggle".into(), args: serde_json::json!({ "name": "settings" }) })
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_toggles_the_settings_panel() {
        let mut w = ControlCenterWidget::new();
        assert_eq!(
            w.on_click(0.0),
            Some(Action::Ipc { cmd: "component.toggle".into(), args: serde_json::json!({ "name": "settings" }) })
        );
    }
}
