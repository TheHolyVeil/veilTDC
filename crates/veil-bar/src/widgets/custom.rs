//! `custom:<name>`: run a command every N seconds and show one line of it.
//!
//! The escape hatch for anything the bar has no widget for. The command runs
//! on a sampler thread with a timeout, so a slow or hung script cannot
//! stall the bar.

use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use veil_text::TextSystem;
use veil_ui::{Metrics, Theme, Widget};

use crate::poll::{Poller, Seen};
use crate::util::{label, run_shell, truncate_chars};

/// Longest line shown, in characters.
const MAX_CHARS: usize = 48;
/// A command that takes longer than this is killed.
const MAX_RUNTIME: Duration = Duration::from_secs(10);

pub struct CustomWidget {
    name: String,
    poll: Poller<Option<String>>,
    seen: Seen,
}

impl CustomWidget {
    pub fn new(name: &str, command: &str, interval_secs: u32) -> Self {
        let command = command.to_string();
        let interval = Duration::from_secs(interval_secs.max(1) as u64);
        let timeout = interval.min(MAX_RUNTIME);
        let poll = Poller::every(&format!("custom-{name}"), interval, move || {
            run_shell(&command, timeout).and_then(|out| first_line(&out))
        });
        Self { name: format!("custom:{name}"), poll, seen: Seen::default() }
    }
}

/// First non-empty line, trimmed and shortened.
pub fn first_line(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| truncate_chars(l, MAX_CHARS))
}

impl Widget for CustomWidget {
    fn name(&self) -> &str {
        &self.name
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
        match self.poll.latest().flatten() {
            Some(line) => label(pixmap, text, m, x, y, h, &line, theme.text_dim),
            None => 0.0,
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_first_non_empty_line() {
        assert_eq!(first_line("\n\n  up 3 hours \nsecond\n").as_deref(), Some("up 3 hours"));
        assert_eq!(first_line("   \n\n"), None);
        assert_eq!(first_line(""), None);
    }

    #[test]
    fn long_lines_are_shortened() {
        let long = "x".repeat(200);
        assert_eq!(first_line(&long).unwrap().chars().count(), MAX_CHARS);
    }

    #[test]
    fn runs_the_command_and_exposes_its_output() {
        let mut w = CustomWidget::new("t", "echo hello; echo ignored", 60);
        assert_eq!(w.name(), "custom:t");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if w.poll.latest().flatten().as_deref() == Some("hello") {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("custom widget never produced its line");
    }

    #[test]
    fn a_failing_command_shows_nothing() {
        let w = CustomWidget::new("f", "exit 1", 60);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            if w.poll.latest().is_some() {
                assert_eq!(w.poll.latest().flatten(), None);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no sample arrived");
    }
}
