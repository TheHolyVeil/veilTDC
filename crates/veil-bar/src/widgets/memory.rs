//! Memory usage monitor widget reading /proc/meminfo.

use std::fs;
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{Theme, Widget};

pub struct MemoryWidget {
    pct: f32,
    last_update: Instant,
}

impl Default for MemoryWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryWidget {
    pub fn new() -> Self {
        let mut w = Self {
            pct: 0.0,
            last_update: Instant::now(),
        };
        w.refresh();
        w
    }

    fn refresh(&mut self) -> bool {
        if let Ok(content) = fs::read_to_string("/proc/meminfo") {
            let mut total = 0u64;
            let mut avail = 0u64;
            for line in content.lines() {
                if line.starts_with("MemTotal:") {
                    total = parse_kb(line);
                } else if line.starts_with("MemAvailable:") {
                    avail = parse_kb(line);
                }
            }
            if total > 0 {
                let used = total.saturating_sub(avail);
                self.pct = (used as f32 / total as f32) * 100.0;
                return true;
            }
        }
        false
    }
}

fn parse_kb(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

impl Widget for MemoryWidget {
    fn name(&self) -> &str {
        "memory"
    }

    fn update(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_update) >= Duration::from_secs(5) {
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
        let ty = y + (h - 12.0) / 2.0;
        let color = if self.pct > 85.0 {
            theme.err
        } else if self.pct > 65.0 {
            theme.warn
        } else {
            theme.text_dim
        };

        let icon = "󰍛 ";
        let label = format!("{icon}{:.0}%", self.pct);
        let drawn = text.draw(pixmap, &label, x + 6.0, ty, Role::UI, 12.0, color);
        drawn + 12.0
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
