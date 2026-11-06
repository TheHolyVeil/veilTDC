//! Toast expiry: which toasts are visible, how long each has left, and when
//! the next one expires. Pure state, the clock is an argument.
//!
//! Rules (proposals, all overridable through `notify.*` config later):
//! - The visible toasts are the newest `max_visible`. Older ones are hidden and
//!   their timers are paused, so nothing expires unseen. When a visible toast
//!   goes, the next older one reappears and its timer resumes where it stopped.
//! - Hovering any toast pauses every timer, so the stack does not reflow under
//!   the pointer.
//! - Critical never expires, timeout `0` never expires, `-1` uses the default,
//!   anything else is clamped to [`MIN_TIMEOUT`]..=[`MAX_TIMEOUT`].
//! - Re-adding an id (`replaces_id`) resets its timer and makes it the newest.
//!
//! Mutating calls advance the clock first and queue any expiries; [`ToastTimers::poll`]
//! is the one place they come out, in the order they happened.

use std::time::{Duration, Instant};

use crate::model::Urgency;

pub const MIN_TIMEOUT: Duration = Duration::from_millis(500);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug)]
pub struct TimerConfig {
    /// Used for a `-1` timeout. `None` means such toasts never expire.
    pub default_timeout: Option<Duration>,
    pub max_visible: usize,
}

impl Default for TimerConfig {
    fn default() -> Self {
        TimerConfig { default_timeout: Some(Duration::from_secs(5)), max_visible: 5 }
    }
}

/// How long a toast lives, or `None` if it stays until dismissed.
pub fn effective_timeout(urgency: Urgency, timeout_ms: i32, default: Option<Duration>) -> Option<Duration> {
    if urgency == Urgency::Critical {
        return None;
    }
    let d = match timeout_ms {
        0 => return None,
        t if t < 0 => default?,
        t => Duration::from_millis(t as u64),
    };
    Some(d.clamp(MIN_TIMEOUT, MAX_TIMEOUT))
}

struct Entry {
    id: u32,
    remaining: Option<Duration>,
}

pub struct ToastTimers {
    cfg: TimerConfig,
    /// Oldest first.
    entries: Vec<Entry>,
    hovered: bool,
    last: Instant,
    expired: Vec<u32>,
}

impl ToastTimers {
    pub fn new(cfg: TimerConfig, now: Instant) -> ToastTimers {
        ToastTimers { cfg: TimerConfig { max_visible: cfg.max_visible.max(1), ..cfg }, entries: Vec::new(), hovered: false, last: now, expired: Vec::new() }
    }

    /// Switch settings on a running clock (a config reload). Toasts already
    /// on screen keep their remaining time; only the visible cap and the
    /// default for later toasts change.
    pub fn set_config(&mut self, cfg: TimerConfig, now: Instant) {
        self.advance_to(now);
        self.cfg = TimerConfig { max_visible: cfg.max_visible.max(1), ..cfg };
    }

    fn first_visible(&self) -> usize {
        self.entries.len().saturating_sub(self.cfg.max_visible)
    }

    /// Run the clock forward to `now`, expiring toasts exactly when they were
    /// due even if `now` is late, so a toast revealed by an expiry only loses
    /// the time it was actually on screen.
    fn advance_to(&mut self, now: Instant) {
        if now <= self.last {
            return;
        }
        let mut left = now - self.last;
        self.last = now;
        while !self.hovered {
            let first = self.first_visible();
            let Some(min) = self.entries[first..].iter().filter_map(|e| e.remaining).min() else { break };
            let step = min.min(left);
            for r in self.entries[first..].iter_mut().filter_map(|e| e.remaining.as_mut()) {
                *r -= step;
            }
            if min > left {
                break;
            }
            left -= step;
            let mut i = first;
            while i < self.entries.len() {
                if self.entries[i].remaining == Some(Duration::ZERO) {
                    let e = self.entries.remove(i);
                    self.expired.push(e.id);
                } else {
                    i += 1;
                }
            }
        }
    }

    /// Show a toast as the newest. An existing entry with the same id is
    /// replaced: new timer, newest position.
    pub fn add(&mut self, id: u32, urgency: Urgency, timeout_ms: i32, now: Instant) {
        self.advance_to(now);
        self.entries.retain(|e| e.id != id);
        let remaining = effective_timeout(urgency, timeout_ms, self.cfg.default_timeout);
        self.entries.push(Entry { id, remaining });
    }

    /// Drop a toast (user dismissed it, or the app closed it). Returns whether it was there.
    pub fn remove(&mut self, id: u32, now: Instant) -> bool {
        self.advance_to(now);
        match self.entries.iter().position(|e| e.id == id) {
            Some(i) => {
                self.entries.remove(i);
                true
            }
            None => false,
        }
    }

    /// The pointer entered or left the toast stack.
    pub fn set_hover(&mut self, on: bool, now: Instant) {
        self.advance_to(now);
        self.hovered = on;
    }

    /// Advance to `now` and return the ids that expired since the last call,
    /// in the order they expired.
    pub fn poll(&mut self, now: Instant) -> Vec<u32> {
        self.advance_to(now);
        std::mem::take(&mut self.expired)
    }

    /// When to wake next, or `None` when nothing is counting down (empty, only
    /// sticky toasts, or hovered). Valid after any call that took `now`.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.hovered {
            return None;
        }
        let min = self.entries[self.first_visible()..].iter().filter_map(|e| e.remaining).min()?;
        Some(self.last + min)
    }

    /// Visible toast ids, newest first (the on-screen order, top to bottom).
    pub fn visible_ids(&self) -> Vec<u32> {
        self.entries[self.first_visible()..].iter().rev().map(|e| e.id).collect()
    }

    /// How many older toasts are waiting off screen.
    pub fn hidden_count(&self) -> usize {
        self.first_visible()
    }

    pub fn contains(&self, id: u32) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_hovered(&self) -> bool {
        self.hovered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S5: Duration = Duration::from_secs(5);

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn timers(max_visible: usize, t0: Instant) -> ToastTimers {
        ToastTimers::new(TimerConfig { default_timeout: Some(S5), max_visible }, t0)
    }

    #[test]
    fn timeout_resolution() {
        let d = Some(S5);
        assert_eq!(effective_timeout(Urgency::Normal, -1, d), Some(S5));
        assert_eq!(effective_timeout(Urgency::Normal, -1, None), None);
        assert_eq!(effective_timeout(Urgency::Normal, 0, d), None);
        assert_eq!(effective_timeout(Urgency::Critical, 3000, d), None);
        assert_eq!(effective_timeout(Urgency::Critical, -1, d), None);
        assert_eq!(effective_timeout(Urgency::Low, 3000, d), Some(Duration::from_secs(3)));
        assert_eq!(effective_timeout(Urgency::Normal, 1, d), Some(MIN_TIMEOUT));
        assert_eq!(effective_timeout(Urgency::Normal, i32::MAX, d), Some(MAX_TIMEOUT));
    }

    #[test]
    fn expires_exactly_at_the_deadline() {
        let t0 = Instant::now();
        let mut t = timers(5, t0);
        t.add(1, Urgency::Normal, -1, t0);
        assert!(t.poll(at(t0, 4999)).is_empty());
        assert_eq!(t.poll(at(t0, 5000)), vec![1]);
        assert!(t.is_empty());
        assert!(t.poll(at(t0, 9000)).is_empty(), "reported once");
    }

    #[test]
    fn sticky_toasts_stay() {
        let t0 = Instant::now();
        let mut t = timers(5, t0);
        t.add(1, Urgency::Critical, -1, t0);
        t.add(2, Urgency::Normal, 0, t0);
        assert!(t.poll(at(t0, 3_600_000)).is_empty());
        assert_eq!(t.next_deadline(), None);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn hover_pauses_and_loses_no_time() {
        let t0 = Instant::now();
        let mut t = timers(5, t0);
        t.add(1, Urgency::Normal, -1, t0);
        t.set_hover(true, at(t0, 2000));
        assert!(t.poll(at(t0, 10_000)).is_empty());
        assert_eq!(t.next_deadline(), None);
        t.set_hover(false, at(t0, 10_000));
        assert!(t.poll(at(t0, 12_999)).is_empty());
        assert_eq!(t.poll(at(t0, 13_000)), vec![1], "the 3 s that were left");
    }

    #[test]
    fn replacing_resets_the_timer_and_moves_to_newest() {
        let t0 = Instant::now();
        let mut t = timers(5, t0);
        t.add(1, Urgency::Normal, -1, t0);
        t.add(2, Urgency::Normal, -1, at(t0, 1000));
        t.add(1, Urgency::Normal, -1, at(t0, 3000));
        assert_eq!(t.visible_ids(), vec![1, 2]);
        assert_eq!(t.poll(at(t0, 6000)), vec![2]);
        assert_eq!(t.poll(at(t0, 8000)), vec![1]);
    }

    #[test]
    fn only_the_newest_are_visible_and_hidden_timers_pause() {
        let t0 = Instant::now();
        let mut t = timers(2, t0);
        t.add(1, Urgency::Normal, -1, t0);
        t.add(2, Urgency::Normal, -1, at(t0, 1000));
        t.add(3, Urgency::Normal, -1, at(t0, 2000));
        assert_eq!(t.visible_ids(), vec![3, 2]);
        assert_eq!(t.hidden_count(), 1);
        // 1 had 3 s left when it was pushed off screen. A single late poll must
        // replay the history: 2 at 6 s, 3 at 7 s, then 1 with its 3 s from 6 s.
        assert_eq!(t.poll(at(t0, 9000)), vec![2, 3, 1]);
        assert!(t.is_empty());
    }

    #[test]
    fn a_late_poll_does_not_eat_the_revealed_toasts_time() {
        let t0 = Instant::now();
        let mut t = timers(1, t0);
        t.add(1, Urgency::Normal, -1, t0);
        t.add(2, Urgency::Normal, -1, t0);
        // 2 is shown from 0 and expires at 5 s; 1 was paused with its full 5 s.
        assert_eq!(t.poll(at(t0, 7000)), vec![2]);
        assert_eq!(t.next_deadline(), Some(at(t0, 7000) + Duration::from_millis(3000)));
        assert_eq!(t.poll(at(t0, 10_000)), vec![1]);
    }

    #[test]
    fn removing_reveals_the_older_toast_and_resumes_it() {
        let t0 = Instant::now();
        let mut t = timers(1, t0);
        t.add(1, Urgency::Normal, -1, t0);
        t.add(2, Urgency::Normal, -1, at(t0, 1000));
        assert!(t.remove(2, at(t0, 2000)));
        assert!(!t.remove(2, at(t0, 2000)));
        assert_eq!(t.visible_ids(), vec![1]);
        assert_eq!(t.next_deadline(), Some(at(t0, 6000)), "1 had 4 s left");
    }

    #[test]
    fn expiry_during_a_mutation_is_queued_for_poll() {
        let t0 = Instant::now();
        let mut t = timers(5, t0);
        t.add(1, Urgency::Normal, -1, t0);
        t.add(2, Urgency::Normal, -1, at(t0, 6000));
        assert_eq!(t.visible_ids(), vec![2]);
        assert_eq!(t.poll(at(t0, 6000)), vec![1]);
    }

    #[test]
    fn next_deadline_tracks_the_soonest_running_toast() {
        let t0 = Instant::now();
        let mut t = timers(5, t0);
        assert_eq!(t.next_deadline(), None);
        t.add(1, Urgency::Normal, 8000, t0);
        t.add(2, Urgency::Normal, 3000, t0);
        assert_eq!(t.next_deadline(), Some(at(t0, 3000)));
        t.poll(at(t0, 3000));
        assert_eq!(t.next_deadline(), Some(at(t0, 8000)));
    }

    #[test]
    fn clock_never_runs_backwards() {
        let t0 = Instant::now();
        let mut t = timers(5, at(t0, 1000));
        t.add(1, Urgency::Normal, -1, at(t0, 1000));
        assert!(t.poll(t0).is_empty());
        assert_eq!(t.poll(at(t0, 6000)), vec![1]);
    }
}
