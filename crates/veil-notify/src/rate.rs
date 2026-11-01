//! Per-app rate limiting: a sliding window per app, with the excess counted so
//! the service can add a single "n suppressed" history entry instead of a flood.
//! The clock is an argument, so tests need no sleeping.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::text::truncate_boundary;

const MAX_APPS: usize = 256;
const MAX_KEY_BYTES: usize = 64;
/// Shared bucket once `MAX_APPS` distinct apps are being tracked, so a sender
/// inventing app names cannot grow the table without bound.
const OVERFLOW_KEY: &str = "*";

#[derive(Clone, Copy, Debug)]
pub struct RateConfig {
    pub max: usize,
    pub window: Duration,
}

impl Default for RateConfig {
    fn default() -> Self {
        RateConfig { max: 20, window: Duration::from_secs(10) }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Show it. `suppressed_before` is how many were dropped since the last
    /// allowed one, for the summary entry; usually 0.
    Allow { suppressed_before: u32 },
    Suppress,
}

#[derive(Default)]
struct AppState {
    stamps: VecDeque<Instant>,
    suppressed: u32,
}

pub struct RateLimiter {
    cfg: RateConfig,
    apps: HashMap<String, AppState>,
}

/// The bucket for a sender: its desktop entry when it gave one, else its name.
pub fn key(app_name: &str, desktop_entry: Option<&str>) -> String {
    let raw = desktop_entry.filter(|s| !s.is_empty()).unwrap_or(app_name);
    truncate_boundary(raw, MAX_KEY_BYTES).0.to_string()
}

impl RateLimiter {
    pub fn new(cfg: RateConfig) -> RateLimiter {
        RateLimiter { cfg: RateConfig { max: cfg.max.max(1), ..cfg }, apps: HashMap::new() }
    }

    fn expire(window: Duration, st: &mut AppState, now: Instant) {
        while st.stamps.front().is_some_and(|&t| now.saturating_duration_since(t) >= window) {
            st.stamps.pop_front();
        }
    }

    pub fn check(&mut self, key: &str, now: Instant) -> Verdict {
        let (max, window) = (self.cfg.max, self.cfg.window);
        if self.apps.len() >= MAX_APPS && !self.apps.contains_key(key) {
            self.prune(now);
        }
        let k = if self.apps.len() >= MAX_APPS && !self.apps.contains_key(key) { OVERFLOW_KEY } else { key };
        let st = self.apps.entry(k.to_string()).or_default();
        Self::expire(window, st, now);
        if st.stamps.len() < max {
            st.stamps.push_back(now);
            Verdict::Allow { suppressed_before: std::mem::take(&mut st.suppressed) }
        } else {
            st.suppressed = st.suppressed.saturating_add(1);
            Verdict::Suppress
        }
    }

    /// Whether some app has dropped notifications that were not yet reported;
    /// the service arms a timer for [`RateLimiter::flush_idle`] while this holds.
    pub fn has_pending(&self) -> bool {
        self.apps.values().any(|s| s.suppressed > 0)
    }

    /// Apps that were suppressed and have since gone quiet for a full window:
    /// returns `(key, dropped)` once each, sorted by key.
    pub fn flush_idle(&mut self, now: Instant) -> Vec<(String, u32)> {
        let window = self.cfg.window;
        let mut out = Vec::new();
        for (k, st) in &mut self.apps {
            Self::expire(window, st, now);
            if st.suppressed > 0 && st.stamps.is_empty() {
                out.push((k.clone(), std::mem::take(&mut st.suppressed)));
            }
        }
        out.sort();
        out
    }

    /// Forget apps with nothing in the window and nothing unreported.
    pub fn prune(&mut self, now: Instant) {
        let window = self.cfg.window;
        self.apps.retain(|_, st| {
            Self::expire(window, st, now);
            !st.stamps.is_empty() || st.suppressed > 0
        });
    }

    pub fn tracked(&self) -> usize {
        self.apps.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim(max: usize, secs: u64) -> RateLimiter {
        RateLimiter::new(RateConfig { max, window: Duration::from_secs(secs) })
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn allows_up_to_max_then_suppresses() {
        let t0 = Instant::now();
        let mut r = lim(3, 10);
        for i in 0..3 {
            assert_eq!(r.check("a", at(t0, i)), Verdict::Allow { suppressed_before: 0 });
        }
        assert_eq!(r.check("a", at(t0, 3)), Verdict::Suppress);
        assert_eq!(r.check("a", at(t0, 4)), Verdict::Suppress);
        assert!(r.has_pending());
    }

    #[test]
    fn window_slides_and_reports_what_was_dropped() {
        let t0 = Instant::now();
        let mut r = lim(2, 10);
        r.check("a", at(t0, 0));
        r.check("a", at(t0, 1000));
        assert_eq!(r.check("a", at(t0, 2000)), Verdict::Suppress);
        assert_eq!(r.check("a", at(t0, 3000)), Verdict::Suppress);
        // the first stamp ages out at 10 s
        assert_eq!(r.check("a", at(t0, 10_000)), Verdict::Allow { suppressed_before: 2 });
        assert!(!r.has_pending());
    }

    #[test]
    fn apps_are_independent() {
        let t0 = Instant::now();
        let mut r = lim(1, 10);
        assert!(matches!(r.check("a", t0), Verdict::Allow { .. }));
        assert_eq!(r.check("a", t0), Verdict::Suppress);
        assert!(matches!(r.check("b", t0), Verdict::Allow { .. }));
    }

    #[test]
    fn quiet_apps_are_flushed_once() {
        let t0 = Instant::now();
        let mut r = lim(1, 10);
        r.check("a", at(t0, 0));
        r.check("a", at(t0, 1));
        r.check("a", at(t0, 2));
        assert!(r.flush_idle(at(t0, 5000)).is_empty(), "still inside the window");
        assert_eq!(r.flush_idle(at(t0, 10_001)), vec![("a".to_string(), 2)]);
        assert!(r.flush_idle(at(t0, 20_000)).is_empty(), "reported once");
        assert!(!r.has_pending());
    }

    #[test]
    fn table_cannot_grow_without_bound() {
        let t0 = Instant::now();
        let mut r = lim(5, 10);
        for i in 0..(MAX_APPS + 50) {
            r.check(&format!("app{i}"), t0);
        }
        assert!(r.tracked() <= MAX_APPS + 1, "{}", r.tracked());
        // once the window passes, idle entries are pruned and new apps fit again
        assert!(matches!(r.check("fresh", at(t0, 60_000)), Verdict::Allow { .. }));
        assert!(r.tracked() < 10);
    }

    #[test]
    fn key_prefers_desktop_entry_and_is_bounded() {
        assert_eq!(key("Mail", Some("org.mail.App")), "org.mail.App");
        assert_eq!(key("Mail", Some("")), "Mail");
        assert_eq!(key("Mail", None), "Mail");
        assert_eq!(key(&"é".repeat(100), None).len(), 64);
    }
}
