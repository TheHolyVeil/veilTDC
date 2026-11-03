//! Notifications: a bell with the unread count. Click opens the history.
//!
//! Driven by the `notifications` IPC event (`{"count": n, "dnd": bool}`),
//! which `veil-notify` publishes. The click asks `notify` to toggle its history
//! panel (`notify.panel`). It must not use `component.toggle`: notify is a
//! persistent service that owns the D-Bus name, and stopping it would make
//! every `notify-send` fail until it is back.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Action, Metrics, Theme, Widget};

use crate::util::button;

#[derive(Default)]
pub struct NotificationsWidget {
    pub count: u32,
    pub dnd: bool,
}

impl NotificationsWidget {
    pub fn new() -> Self {
        Self::default()
    }

    /// The glyph and label for a state: bell or bell-off, with the count.
    fn text(&self) -> String {
        let icon = if self.dnd { "\u{f09a7}" } else { "\u{f09a4}" };
        if self.count > 0 {
            format!("{icon} {}", self.count.min(99))
        } else {
            icon.to_string()
        }
    }
}

impl Widget for NotificationsWidget {
    fn name(&self) -> &str {
        "notifications"
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
        let color = if self.dnd {
            theme.text_dim
        } else if self.count > 0 {
            theme.accent
        } else {
            theme.text
        };
        // A pill only while something is waiting (and not muted).
        let pill = self.count > 0 && !self.dnd;
        button(pixmap, text, theme, m, x, y, h, &self.text(), color, pill)
    }

    fn on_click(&mut self, _x: f32) -> Option<Action> {
        Some(Action::Ipc { cmd: veil_ipc::commands::NOTIFY_PANEL.into(), args: serde_json::json!({ "mode": "toggle" }) })
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_toggles_the_panel_and_never_the_service() {
        let mut w = NotificationsWidget::new();
        match w.on_click(0.0) {
            Some(Action::Ipc { cmd, args }) => {
                assert_eq!(cmd, "notify.panel");
                assert_eq!(args, serde_json::json!({ "mode": "toggle" }));
            }
            _ => panic!("the bell must send an IPC command"),
        }
    }

    #[test]
    fn label_follows_count_and_dnd() {
        let mut w = NotificationsWidget::new();
        assert_eq!(w.text(), "\u{f09a4}");
        w.count = 3;
        assert_eq!(w.text(), "\u{f09a4} 3");
        w.count = 250;
        assert_eq!(w.text(), "\u{f09a4} 99", "capped so the bar does not jump");
        w.dnd = true;
        assert_eq!(w.text(), "\u{f09a7} 99");
    }
}
