//! Battery percentage and charging status widget.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{Theme, Widget};

pub struct BatteryWidget {
    pct: u8,
    charging: bool,
    present: bool,
    last_update: Instant,
}

impl Default for BatteryWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl BatteryWidget {
    pub fn new() -> Self {
        let mut w = Self {
            pct: 100,
            charging: false,
            present: false,
            last_update: Instant::now(),
        };
        w.refresh();
        w
    }

    fn refresh(&mut self) -> bool {
        let bat_dir = Path::new("/sys/class/power_supply");
        if let Ok(entries) = fs::read_dir(bat_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("BAT") {
                    self.present = true;
                    let path = entry.path();
                    if let Ok(cap) = fs::read_to_string(path.join("capacity")) {
                        self.pct = cap.trim().parse().unwrap_or(self.pct);
                    }
                    if let Ok(status) = fs::read_to_string(path.join("status")) {
                        self.charging = status.trim().eq_ignore_ascii_case("Charging");
                    }
                    return true;
                }
            }
        }
        self.present = false;
        false
    }
}

impl Widget for BatteryWidget {
    fn name(&self) -> &str {
        "battery"
    }

    fn update(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_update) >= Duration::from_secs(30) {
            self.last_update = now;
            self.refresh()
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
        if !self.present {
            return 0.0;
        }

        let ty = y + (h - 12.0) / 2.0;
        let color = if self.charging {
            theme.ok
        } else if self.pct <= 15 {
            theme.err
        } else if self.pct <= 30 {
            theme.warn
        } else {
            theme.text_dim
        };

        let icon = if self.charging { "󰂄 " } else { "󰁹 " };
        let label = format!("{icon}{}%", self.pct);
        let drawn = text.draw(pixmap, &label, x + 6.0, ty, Role::UI, 12.0, color);
        drawn + 12.0
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
