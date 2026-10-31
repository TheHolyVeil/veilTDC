//! Bar settings as they travel over IPC.
//!
//! The compositor reads `bar = { ... }` from Lua; components never link Lua
//! (section 8), so the bar receives this typed form in the `hello` reply and
//! in `config` events. Every field has a default, so a partial or older
//! payload still deserialises, and [`BarSettings::sanitized`] puts values
//! back into their valid range instead of trusting the sender.

use serde::{Deserialize, Serialize};

/// Bar height limits in logical px (section 8 schema sketch).
pub const BAR_HEIGHT_MIN: u32 = 24;
pub const BAR_HEIGHT_MAX: u32 = 64;
pub const BAR_HEIGHT_DEFAULT: u32 = 36;
/// Custom widgets may not poll faster than this (seconds).
pub const CUSTOM_INTERVAL_MIN: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BarSettings {
    /// `"top"` or `"bottom"`.
    pub position: String,
    /// Height in logical px at the default font size; scales with `font.size`.
    pub height: u32,
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
    /// `strftime` format of the clock widget.
    pub clock_format: String,
    /// Mount point the disk widget reports.
    pub disk_mount: String,
    /// Weather needs coordinates; there is deliberately no auto-location.
    pub weather: Option<WeatherSettings>,
    /// `custom:<name>` widgets.
    pub custom: Vec<CustomWidget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherSettings {
    pub lat: f64,
    pub lon: f64,
    /// `"metric"` (°C) or `"imperial"` (°F).
    #[serde(default = "default_units")]
    pub units: String,
}

fn default_units() -> String {
    "metric".into()
}

/// One `custom:<name>` widget: run `command` every `interval` seconds and
/// show the first line of its output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomWidget {
    pub name: String,
    pub command: String,
    #[serde(default = "default_interval")]
    pub interval: u32,
}

fn default_interval() -> u32 {
    10
}

impl Default for BarSettings {
    fn default() -> Self {
        Self {
            position: "top".into(),
            height: BAR_HEIGHT_DEFAULT,
            left: vec!["workspaces".into(), "window_title".into()],
            center: vec!["clock".into()],
            right: vec!["cpu".into(), "memory".into(), "battery".into(), "audio".into()],
            clock_format: "%a %b %d  %H:%M".into(),
            disk_mount: "/".into(),
            weather: None,
            custom: Vec::new(),
        }
    }
}

impl BarSettings {
    pub fn at_top(&self) -> bool {
        !self.position.eq_ignore_ascii_case("bottom")
    }

    /// Clamp everything into its valid range. Unknown widget names are kept
    /// (the bar skips them with a warning), blank ones are dropped.
    pub fn sanitized(mut self) -> Self {
        self.height = self.height.clamp(BAR_HEIGHT_MIN, BAR_HEIGHT_MAX);
        self.position = if self.at_top() { "top".into() } else { "bottom".into() };
        for section in [&mut self.left, &mut self.center, &mut self.right] {
            section.retain(|n| !n.trim().is_empty());
            for n in section.iter_mut() {
                *n = n.trim().to_string();
            }
        }
        if self.clock_format.trim().is_empty() {
            self.clock_format = Self::default().clock_format;
        }
        if self.disk_mount.trim().is_empty() {
            self.disk_mount = "/".into();
        }
        if let Some(w) = &mut self.weather {
            w.lat = w.lat.clamp(-90.0, 90.0);
            w.lon = w.lon.clamp(-180.0, 180.0);
            w.units = if w.units.eq_ignore_ascii_case("imperial") { "imperial".into() } else { "metric".into() };
        }
        self.custom.retain(|c| !c.name.trim().is_empty() && !c.command.trim().is_empty());
        for c in &mut self.custom {
            c.interval = c.interval.max(CUSTOM_INTERVAL_MIN);
        }
        self
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Lenient: anything that is not an object gives the defaults.
    pub fn from_json(v: &serde_json::Value) -> Self {
        serde_json::from_value::<Self>(v.clone()).unwrap_or_default().sanitized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_the_doc_layout() {
        let d = BarSettings::default();
        assert_eq!(d.height, 36);
        assert_eq!(d.left, ["workspaces", "window_title"]);
        assert_eq!(d.center, ["clock"]);
        assert!(d.at_top());
    }

    #[test]
    fn json_round_trip() {
        let s = BarSettings {
            position: "bottom".into(),
            height: 44,
            left: vec!["activities".into()],
            weather: Some(WeatherSettings { lat: 48.1, lon: 11.5, units: "imperial".into() }),
            custom: vec![CustomWidget { name: "up".into(), command: "uptime -p".into(), interval: 30 }],
            ..Default::default()
        };
        assert_eq!(BarSettings::from_json(&s.to_json()), s);
    }

    #[test]
    fn partial_and_garbage_payloads_fall_back_to_defaults() {
        assert_eq!(BarSettings::from_json(&json!({})), BarSettings::default());
        assert_eq!(BarSettings::from_json(&json!(null)), BarSettings::default());
        assert_eq!(BarSettings::from_json(&json!("nope")), BarSettings::default());
        let s = BarSettings::from_json(&json!({ "height": 40 }));
        assert_eq!(s.height, 40);
        assert_eq!(s.center, ["clock"], "unmentioned fields keep their defaults");
    }

    #[test]
    fn sanitizing_clamps_and_cleans() {
        let s = BarSettings::from_json(&json!({
            "height": 500,
            "position": "sideways",
            "left": ["  clock ", "", "cpu"],
            "weather": { "lat": 999.0, "lon": -999.0, "units": "kelvin" },
            "custom": [
                { "name": "ok", "command": "true", "interval": 0 },
                { "name": "", "command": "true" },
                { "name": "nocmd", "command": "  " }
            ],
        }));
        assert_eq!(s.height, BAR_HEIGHT_MAX);
        assert_eq!(s.position, "top");
        assert_eq!(s.left, ["clock", "cpu"]);
        let w = s.weather.unwrap();
        assert_eq!((w.lat, w.lon, w.units.as_str()), (90.0, -180.0, "metric"));
        assert_eq!(s.custom.len(), 1);
        assert_eq!(s.custom[0].interval, CUSTOM_INTERVAL_MIN);
        assert_eq!(BarSettings::from_json(&json!({ "height": 1 })).height, BAR_HEIGHT_MIN);
    }
}
