//! CPU (and GPU, when there is one) temperature from hwmon.
//!
//! Sensors are found by driver *name*, not hwmon index, because the index
//! shifts between boots. sysfs reads are quick, so this runs on the render
//! thread every few seconds.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Metrics, Theme, Widget};

use crate::util::{label, level_color};

/// hwmon drivers that report a CPU temperature, best first.
const CPU_DRIVERS: [&str; 5] = ["k10temp", "coretemp", "zenpower", "cpu_thermal", "acpitz"];

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Temps {
    pub cpu: Option<f32>,
    pub gpu: Option<f32>,
}

pub struct TempWidget {
    temps: Temps,
    last: Option<Instant>,
}

impl Default for TempWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl TempWidget {
    pub fn new() -> Self {
        Self { temps: Temps::default(), last: None }
    }
}

impl Widget for TempWidget {
    fn name(&self) -> &str {
        "temp"
    }

    fn update(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|l| now.duration_since(l) < Duration::from_secs(5)) {
            return false;
        }
        self.last = Some(now);
        let fresh = read_temps(Path::new("/sys/class/hwmon"), Path::new("/sys/class/thermal"));
        let changed = fresh != self.temps;
        self.temps = fresh;
        changed
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
        let mut used = 0.0;
        for (icon, t) in [("󰔏", self.temps.cpu), ("󰢮", self.temps.gpu)] {
            if let Some(c) = t {
                let s = format!("{icon} {c:.0}°");
                used += label(pixmap, text, m, x + used, y, h, &s, level_color(theme, c, 70.0, 85.0));
            }
        }
        used
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn read_millideg(path: &Path) -> Option<f32> {
    let raw: f32 = fs::read_to_string(path).ok()?.trim().parse().ok()?;
    Some(raw / 1000.0)
}

fn hwmon_by_name(root: &Path, driver: &str) -> Option<PathBuf> {
    fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| fs::read_to_string(p.join("name")).is_ok_and(|n| n.trim() == driver))
}

pub fn read_temps(hwmon: &Path, thermal: &Path) -> Temps {
    let cpu = CPU_DRIVERS
        .iter()
        .find_map(|d| hwmon_by_name(hwmon, d).and_then(|p| read_millideg(&p.join("temp1_input"))))
        .or_else(|| {
            // No hwmon CPU sensor: the package thermal zone, if there is one.
            fs::read_dir(thermal).ok()?.flatten().map(|e| e.path()).find_map(|zone| {
                let kind = fs::read_to_string(zone.join("type")).ok()?;
                (kind.trim() == "x86_pkg_temp").then(|| read_millideg(&zone.join("temp"))).flatten()
            })
        });

    // amdgpu: temp1 is the edge, temp2 the junction (the hot spot).
    let gpu = hwmon_by_name(hwmon, "amdgpu")
        .and_then(|p| read_millideg(&p.join("temp2_input")).or_else(|| read_millideg(&p.join("temp1_input"))))
        .or_else(|| hwmon_by_name(hwmon, "nouveau").and_then(|p| read_millideg(&p.join("temp1_input"))));

    Temps { cpu, gpu }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensor(root: &Path, dir: &str, name: &str, files: &[(&str, &str)]) {
        let d = root.join(dir);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("name"), format!("{name}\n")).unwrap();
        for (f, v) in files {
            fs::write(d.join(f), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn finds_sensors_by_driver_name_not_index() {
        let t = tempfile::tempdir().unwrap();
        let hw = t.path().join("hwmon");
        sensor(&hw, "hwmon7", "nvme", &[("temp1_input", "41000")]);
        sensor(&hw, "hwmon3", "k10temp", &[("temp1_input", "52500")]);
        sensor(&hw, "hwmon1", "amdgpu", &[("temp1_input", "44000"), ("temp2_input", "61000")]);
        let r = read_temps(&hw, &t.path().join("thermal"));
        assert_eq!(r, Temps { cpu: Some(52.5), gpu: Some(61.0) }, "junction temp, not edge");
    }

    #[test]
    fn prefers_k10temp_over_acpitz() {
        let t = tempfile::tempdir().unwrap();
        let hw = t.path().join("hwmon");
        sensor(&hw, "hwmon0", "acpitz", &[("temp1_input", "27800")]);
        sensor(&hw, "hwmon1", "coretemp", &[("temp1_input", "60000")]);
        assert_eq!(read_temps(&hw, &t.path().join("x")).cpu, Some(60.0));
    }

    #[test]
    fn falls_back_to_the_package_thermal_zone() {
        let t = tempfile::tempdir().unwrap();
        let zone = t.path().join("thermal/thermal_zone2");
        fs::create_dir_all(&zone).unwrap();
        fs::write(zone.join("type"), "x86_pkg_temp\n").unwrap();
        fs::write(zone.join("temp"), "48000\n").unwrap();
        assert_eq!(read_temps(&t.path().join("none"), &t.path().join("thermal")).cpu, Some(48.0));
    }

    #[test]
    fn no_sensors_means_nothing_shown() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(read_temps(t.path(), t.path()), Temps::default());
    }

    #[test]
    fn gpu_without_junction_falls_back_to_edge() {
        let t = tempfile::tempdir().unwrap();
        let hw = t.path().join("hwmon");
        sensor(&hw, "hwmon1", "amdgpu", &[("temp1_input", "44000")]);
        assert_eq!(read_temps(&hw, t.path()).gpu, Some(44.0));
    }
}
