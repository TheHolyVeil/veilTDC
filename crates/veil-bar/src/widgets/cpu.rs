//! CPU usage monitor widget.

use std::fs;
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{Theme, Widget};

pub struct CpuWidget {
    prev_idle: u64,
    prev_total: u64,
    pct: f32,
    last_update: Instant,
}

impl Default for CpuWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuWidget {
    pub fn new() -> Self {
        let (idle, total) = read_cpu_stat();
        Self {
            prev_idle: idle,
            prev_total: total,
            pct: 0.0,
            last_update: Instant::now(),
        }
    }

    fn refresh(&mut self) -> bool {
        let (idle, total) = read_cpu_stat();
        let d_total = total.saturating_sub(self.prev_total);
        let d_idle = idle.saturating_sub(self.prev_idle);
        if d_total > 0 {
            self.pct = (d_total - d_idle) as f32 / d_total as f32 * 100.0;
        }
        self.prev_idle = idle;
        self.prev_total = total;
        true
    }
}

impl Widget for CpuWidget {
    fn name(&self) -> &str {
        "cpu"
    }

    fn update(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_update) >= Duration::from_secs(2) {
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
        let color = if self.pct > 80.0 {
            theme.err
        } else if self.pct > 50.0 {
            theme.warn
        } else {
            theme.text_dim
        };

        // Icon + percentage
        let icon = " ";
        let label = format!("{icon}{:.0}%", self.pct);
        let drawn = text.draw(pixmap, &label, x + 6.0, ty, Role::UI, 12.0, color);
        drawn + 12.0
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn read_cpu_stat() -> (u64, u64) {
    if let Ok(content) = fs::read_to_string("/proc/stat") {
        if let Some(line) = content.lines().next() {
            let mut parts = line.split_whitespace().skip(1);
            let user: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let nice: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let system: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let idle: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let iowait: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let irq: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let softirq: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let steal: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);

            let total = user + nice + system + idle + iowait + irq + softirq + steal;
            return (idle, total);
        }
    }
    (0, 0)
}
