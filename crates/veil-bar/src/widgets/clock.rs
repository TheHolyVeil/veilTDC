//! Clock and calendar widget with formatted date and time.

use chrono::{Local, TimeZone};
use std::time::Instant;
use tiny_skia::Pixmap;
use veil_text::{Role, TextSystem};
use veil_ui::{Metrics, Theme, Widget};

const DEFAULT_FORMAT: &str = "%a %b %d  %H:%M";

pub struct ClockWidget {
    format: String,
    last_rendered: String,
}

impl Default for ClockWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl ClockWidget {
    pub fn new() -> Self {
        Self::with_format(DEFAULT_FORMAT)
    }

    /// `format` is a `strftime` string. A broken one (chrono reports the
    /// error when formatting) falls back to the default instead of panicking.
    pub fn with_format(format: &str) -> Self {
        Self {
            format: format.to_string(),
            last_rendered: String::new(),
        }
    }

    fn render_at<Tz: TimeZone>(&self, t: &chrono::DateTime<Tz>) -> String
    where
        Tz::Offset: std::fmt::Display,
    {
        use std::fmt::Write;
        let mut out = String::new();
        if write!(out, "{}", t.format(&self.format)).is_err() {
            out = t.format(DEFAULT_FORMAT).to_string();
        }
        out
    }

    fn now_string(&self) -> String {
        self.render_at(&Local::now())
    }
}

impl Widget for ClockWidget {
    fn name(&self) -> &str {
        "clock"
    }

    fn update(&mut self, _now: Instant) -> bool {
        let current = self.now_string();
        if current != self.last_rendered {
            self.last_rendered = current;
            true
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
        let current = self.now_string();
        let ty = y + (h - m.font) / 2.0;
        let drawn = text.draw(pixmap, &current, x + m.pad, ty, Role::Mono, m.font, theme.text);
        drawn + m.pad * 2.0
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn formats_with_the_configured_strftime_string() {
        let t = Utc.with_ymd_and_hms(2026, 10, 30, 14, 5, 9).unwrap();
        assert_eq!(ClockWidget::with_format("%H:%M").render_at(&t), "14:05");
        assert_eq!(ClockWidget::with_format("%Y-%m-%d").render_at(&t), "2026-10-30");
    }

    #[test]
    fn an_invalid_format_does_not_panic() {
        let t = Utc.with_ymd_and_hms(2026, 10, 30, 14, 5, 9).unwrap();
        let s = ClockWidget::with_format("%Q%").render_at(&t);
        assert_eq!(s, "Fri Oct 30  14:05");
    }
}
