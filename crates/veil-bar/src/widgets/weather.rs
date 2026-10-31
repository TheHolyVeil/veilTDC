//! Weather: icon and temperature from Open-Meteo (free, no API key).
//!
//! Needs coordinates in `bar.widgets.weather`; there is deliberately no
//! auto-location. The request runs on a sampler thread with a timeout and the
//! widget stays hidden until the first answer, so a slow or absent network
//! never delays or stalls the bar.

use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_ipc::WeatherSettings;
use veil_text::TextSystem;
use veil_ui::{Metrics, Theme, Widget};

use crate::poll::{Poller, Seen};
use crate::util::label;

const REFRESH: Duration = Duration::from_secs(15 * 60);
const RETRY: Duration = Duration::from_secs(2 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub temp: f32,
    pub code: u32,
}

pub struct WeatherWidget {
    imperial: bool,
    poll: Poller<Option<Reading>>,
    seen: Seen,
}

impl WeatherWidget {
    pub fn new(s: &WeatherSettings) -> Self {
        let imperial = s.units == "imperial";
        let url = request_url(s);
        let mut last_good: Option<Reading> = None;
        let poll = Poller::spawn("weather", move || match fetch(&url) {
            Some(r) => {
                last_good = Some(r);
                (last_good, REFRESH)
            }
            // Keep showing the last good reading, and try again sooner.
            None => (last_good, RETRY),
        });
        Self { imperial, poll, seen: Seen::default() }
    }
}

pub fn request_url(s: &WeatherSettings) -> String {
    let unit = if s.units == "imperial" { "fahrenheit" } else { "celsius" };
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current=temperature_2m,weather_code&temperature_unit={unit}",
        s.lat, s.lon
    )
}

fn fetch(url: &str) -> Option<Reading> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into();
    let body = agent.get(url).call().ok()?.body_mut().read_to_string().ok()?;
    parse_current(&body)
}

/// `{"current":{"temperature_2m":21.3,"weather_code":2,...}}`
pub fn parse_current(body: &str) -> Option<Reading> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let cur = v.get("current")?;
    Some(Reading {
        temp: cur.get("temperature_2m")?.as_f64()? as f32,
        code: cur.get("weather_code")?.as_u64()? as u32,
    })
}

/// WMO weather code to a Nerd Font icon.
pub fn wmo_icon(code: u32) -> &'static str {
    match code {
        0 => "\u{f0599}",                 // clear sky
        1..=3 => "\u{f0595}",             // partly cloudy
        45 | 48 => "\u{f0591}",           // fog
        51..=57 | 61..=67 | 80..=82 => "\u{f0597}", // drizzle, rain, showers
        71..=77 | 85 | 86 => "\u{f059b}", // snow
        95..=99 => "\u{f0593}",           // thunderstorm
        _ => "\u{f0590}",                 // cloudy / unknown
    }
}

impl Widget for WeatherWidget {
    fn name(&self) -> &str {
        "weather"
    }

    fn update(&mut self, _now: Instant) -> bool {
        self.seen.changed(&self.poll)
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
        let Some(Some(r)) = self.poll.latest() else {
            return 0.0;
        };
        let unit = if self.imperial { "°F" } else { "°C" };
        let s = format!("{} {:.0}{unit}", wmo_icon(r.code), r.temp);
        label(pixmap, text, m, x, y, h, &s, theme.text)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_open_meteo_reply() {
        let body = r#"{"latitude":48.1,"current":{"time":"2026-10-30T12:00","temperature_2m":12.4,"weather_code":61}}"#;
        assert_eq!(parse_current(body), Some(Reading { temp: 12.4, code: 61 }));
    }

    #[test]
    fn rejects_garbage_and_partial_replies() {
        assert_eq!(parse_current("not json"), None);
        assert_eq!(parse_current(r#"{"current":{"temperature_2m":1.0}}"#), None);
        assert_eq!(parse_current(r#"{"error":true}"#), None);
    }

    #[test]
    fn url_carries_coordinates_and_units() {
        let s = WeatherSettings { lat: 48.14, lon: 11.58, units: "imperial".into() };
        let u = request_url(&s);
        assert!(u.contains("latitude=48.14") && u.contains("longitude=11.58"));
        assert!(u.ends_with("temperature_unit=fahrenheit"));
        let metric = WeatherSettings { units: "metric".into(), ..s };
        assert!(request_url(&metric).ends_with("temperature_unit=celsius"));
    }

    #[test]
    fn icons_cover_the_wmo_groups() {
        assert_eq!(wmo_icon(0), "\u{f0599}");
        assert_eq!(wmo_icon(63), wmo_icon(81), "rain and showers share an icon");
        assert_eq!(wmo_icon(73), "\u{f059b}");
        assert_eq!(wmo_icon(96), "\u{f0593}");
        assert_eq!(wmo_icon(1234), "\u{f0590}", "unknown codes fall back to cloudy");
    }
}
