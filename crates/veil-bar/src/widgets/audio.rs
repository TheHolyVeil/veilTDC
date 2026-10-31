//! Audio volume widget showing volume and mute status from Veil IPC.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Action, Metrics, Theme, Widget};

pub struct AudioWidget {
    pub volume: u8,
    pub muted: bool,
}

impl Default for AudioWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioWidget {
    pub fn new() -> Self {
        Self {
            volume: 50,
            muted: false,
        }
    }
}

impl Widget for AudioWidget {
    fn name(&self) -> &str {
        "audio"
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
        let color = if self.muted {
            theme.err
        } else {
            theme.text_dim
        };

        let icon = if self.muted { "󰖁 " } else { "󰕾 " };
        let label = if self.muted {
            format!("{icon}mute")
        } else {
            format!("{icon}{}%", self.volume)
        };

        crate::util::label(pixmap, text, m, x, y, h, &label, color)
    }

    /// Click toggles mute.
    fn on_click(&mut self, _x: f32) -> Option<Action> {
        Some(Action::Ipc { cmd: "volume.step".into(), args: serde_json::json!({ "mute": true }) })
    }

    /// Scrolling up raises the volume, down lowers it (5% per notch).
    fn on_scroll(&mut self, dy: f32) -> Option<Action> {
        if dy == 0.0 {
            return None;
        }
        let delta = if dy < 0.0 { 5 } else { -5 };
        Some(Action::Ipc { cmd: "volume.step".into(), args: serde_json::json!({ "delta": delta }) })
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
