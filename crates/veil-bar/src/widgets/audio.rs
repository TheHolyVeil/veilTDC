//! Audio volume widget showing volume and mute status from Veil IPC.

use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{Theme, Widget};

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
        x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        let ty = y + (h - 12.0) / 2.0;
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

        let drawn = text.draw(pixmap, &label, x + 6.0, ty, Role::UI, 12.0, color);
        drawn + 12.0
    }

    fn on_click(&mut self, _x: f32, _y: f32) -> Option<String> {
        // Toggle mute or launch volume control
        Some("volume.toggle_mute".into())
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
