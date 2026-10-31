//! Battery percentage, charging status and time left widget.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Metrics, Theme, Widget};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatteryInfo {
    pub pct: u8,
    pub charging: bool,
    pub time_left: Option<(u32, u32)>, // (hours, minutes)
}

pub struct BatteryWidget {
    info: Option<BatteryInfo>,
    sysfs_path: Option<std::path::PathBuf>,
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
            info: None,
            sysfs_path: None,
            last_update: Instant::now(),
        };
        w.refresh();
        w
    }

    #[cfg(test)]
    pub fn with_sysfs(path: impl Into<std::path::PathBuf>) -> Self {
        let mut w = Self {
            info: None,
            sysfs_path: Some(path.into()),
            last_update: Instant::now(),
        };
        w.refresh();
        w
    }

    fn refresh(&mut self) -> bool {
        let base = self
            .sysfs_path
            .as_deref()
            .unwrap_or_else(|| Path::new("/sys/class/power_supply"));
        let new_info = read_battery_info(base);
        if new_info != self.info {
            self.info = new_info;
            true
        } else {
            false
        }
    }
}

pub fn read_battery_info(base: &Path) -> Option<BatteryInfo> {
    let entries = fs::read_dir(base).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("BAT") {
            let path = entry.path();
            let pct = fs::read_to_string(path.join("capacity"))
                .ok()?
                .trim()
                .parse::<u8>()
                .ok()?
                .min(100);
            let status = fs::read_to_string(path.join("status")).unwrap_or_default();
            let st = status.trim();
            let charging = st.eq_ignore_ascii_case("Charging") || st.eq_ignore_ascii_case("Full");
            let time_left = read_time_left(&path, charging);
            return Some(BatteryInfo {
                pct,
                charging,
                time_left,
            });
        }
    }
    None
}

fn read_time_left(path: &Path, charging: bool) -> Option<(u32, u32)> {
    let read_val = |name: &str| -> Option<f64> {
        fs::read_to_string(path.join(name)).ok()?.trim().parse::<f64>().ok()
    };

    // Prefer energy_now / power_now, fall back to charge_now / current_now
    let (now, rate, full) = if let (Some(e_now), Some(p_now)) = (read_val("energy_now"), read_val("power_now")) {
        (e_now, p_now, read_val("energy_full"))
    } else if let (Some(c_now), Some(i_now)) = (read_val("charge_now"), read_val("current_now")) {
        (c_now, i_now, read_val("charge_full"))
    } else {
        return None;
    };

    if rate <= 0.0 {
        return None;
    }

    let hours = if charging {
        let full = full?;
        if full <= now {
            return None;
        }
        (full - now) / rate
    } else {
        now / rate
    };

    if hours <= 0.0 || hours > 100.0 {
        return None;
    }

    let total_mins = (hours * 60.0).round() as u32;
    Some((total_mins / 60, total_mins % 60))
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
        m: &Metrics,
        x: f32,
        y: f32,
        h: f32,
    ) -> f32 {
        let Some(info) = &self.info else {
            return 0.0;
        };

        let color = if info.charging {
            theme.ok
        } else if info.pct <= 15 {
            theme.err
        } else if info.pct <= 30 {
            theme.warn
        } else {
            theme.text_dim
        };

        let icon = if info.charging {
            "\u{f0084} " // nf-md-battery_charging
        } else {
            match info.pct {
                80..=100 => "\u{f0079} ", // nf-md-battery
                60..=79 => "\u{f007a} ",
                40..=59 => "\u{f007b} ",
                20..=39 => "\u{f007c} ",
                _ => "\u{f007d} ", // nf-md-battery_alert
            }
        };

        let label = if let Some((h_left, m_left)) = info.time_left {
            format!("{icon}{}% ({h_left}h{m_left:02}m)", info.pct)
        } else {
            format!("{icon}{}%", info.pct)
        };
        crate::util::label(pixmap, text, m, x, y, h, &label, color)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_battery_returns_none() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(read_battery_info(t.path()), None);
    }

    #[test]
    fn discharging_battery_with_energy_calculates_time_left() {
        let t = tempfile::tempdir().unwrap();
        let bat = t.path().join("BAT0");
        fs::create_dir_all(&bat).unwrap();
        fs::write(bat.join("capacity"), "50\n").unwrap();
        fs::write(bat.join("status"), "Discharging\n").unwrap();
        // 50 Wh remaining, discharging at 20 W => 2.5 hours = 2h 30m
        fs::write(bat.join("energy_now"), "50000000\n").unwrap();
        fs::write(bat.join("power_now"), "20000000\n").unwrap();

        let info = read_battery_info(t.path()).unwrap();
        assert_eq!(info.pct, 50);
        assert!(!info.charging);
        assert_eq!(info.time_left, Some((2, 30)));
    }

    #[test]
    fn charging_battery_with_energy_calculates_time_to_full() {
        let t = tempfile::tempdir().unwrap();
        let bat = t.path().join("BAT0");
        fs::create_dir_all(&bat).unwrap();
        fs::write(bat.join("capacity"), "80\n").unwrap();
        fs::write(bat.join("status"), "Charging\n").unwrap();
        // Full: 100 Wh, Now: 80 Wh, charging at 20 W => 1 hour = 1h 00m
        fs::write(bat.join("energy_full"), "100000000\n").unwrap();
        fs::write(bat.join("energy_now"), "80000000\n").unwrap();
        fs::write(bat.join("power_now"), "20000000\n").unwrap();

        let info = read_battery_info(t.path()).unwrap();
        assert_eq!(info.pct, 80);
        assert!(info.charging);
        assert_eq!(info.time_left, Some((1, 0)));
    }

    #[test]
    fn charge_and_current_fallback() {
        let t = tempfile::tempdir().unwrap();
        let bat = t.path().join("BAT1");
        fs::create_dir_all(&bat).unwrap();
        fs::write(bat.join("capacity"), "75\n").unwrap();
        fs::write(bat.join("status"), "Discharging\n").unwrap();
        // 3000 mAh now, 1000 mA draw => 3 hours
        fs::write(bat.join("charge_now"), "3000000\n").unwrap();
        fs::write(bat.join("current_now"), "1000000\n").unwrap();

        let info = read_battery_info(t.path()).unwrap();
        assert_eq!(info.pct, 75);
        assert_eq!(info.time_left, Some((3, 0)));
    }
}
