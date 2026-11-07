//! What the OSD shows and when, with no Wayland and no clock of its own.
//! Every function takes `now`, so the tests run without waiting.

use std::time::{Duration, Instant};

use veil_ipc::osd::OsdEvent;

/// Fade in on appearing: short, because a volume key should feel instant.
pub const FADE_IN: Duration = Duration::from_millis(60);
/// Fade out before expiring: gentle, nobody is waiting on it.
pub const FADE_OUT: Duration = Duration::from_millis(120);
/// Redraw interval while a fade runs (about 60 fps).
pub const FRAME: Duration = Duration::from_millis(16);

struct Active {
    event: OsdEvent,
    /// When it first appeared. A later event while it is still up only
    /// replaces the content and the deadline: the pill does not fade in again,
    /// so holding a volume key looks like one steady pill.
    appeared: Instant,
    expires: Instant,
}

#[derive(Default)]
pub struct Model {
    active: Option<Active>,
}

impl Model {
    pub fn show(&mut self, event: OsdEvent, now: Instant) {
        let expires = now + Duration::from_millis(u64::from(event.timeout_ms));
        let appeared = match &self.active {
            Some(a) if now < a.expires => a.appeared,
            _ => now,
        };
        self.active = Some(Active { event, appeared, expires });
    }

    pub fn current(&self) -> Option<&OsdEvent> {
        self.active.as_ref().map(|a| &a.event)
    }

    /// Forget what was showing (surface lost, font changed).
    pub fn clear(&mut self) {
        self.active = None;
    }

    /// Drop the OSD once its time is up. True when that just happened.
    pub fn expire(&mut self, now: Instant) -> bool {
        match &self.active {
            Some(a) if now >= a.expires => {
                self.active = None;
                true
            }
            _ => false,
        }
    }

    /// 0.0 to 1.0, ease-out, from the fade-in and the fade-out.
    pub fn alpha(&self, now: Instant) -> f32 {
        let Some(a) = &self.active else { return 0.0 };
        let t_in = now.saturating_duration_since(a.appeared).as_secs_f32() / FADE_IN.as_secs_f32();
        let t_out = a.expires.saturating_duration_since(now).as_secs_f32() / FADE_OUT.as_secs_f32();
        ease_out(t_in.min(t_out).clamp(0.0, 1.0))
    }

    /// How long until something needs doing (a frame of a fade, the start of
    /// the fade-out, or the expiry). `None` when nothing is showing.
    pub fn next_wake(&self, now: Instant) -> Option<Duration> {
        let a = self.active.as_ref()?;
        let fading_in = now < a.appeared + FADE_IN;
        let fade_out_at = a.expires.checked_sub(FADE_OUT).unwrap_or(a.expires);
        if fading_in || now >= fade_out_at {
            return Some(FRAME.min(a.expires.saturating_duration_since(now)));
        }
        Some(fade_out_at.saturating_duration_since(now))
    }
}

/// Cubic ease-out.
fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(timeout_ms: u32) -> OsdEvent {
        OsdEvent::new("Volume", "40%", Some(40), timeout_ms, "")
    }
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn empty_model_is_invisible_and_idle() {
        let m = Model::default();
        let t = Instant::now();
        assert!(m.current().is_none());
        assert_eq!(m.alpha(t), 0.0);
        assert_eq!(m.next_wake(t), None);
    }

    #[test]
    fn fades_in_holds_then_fades_out() {
        let t = Instant::now();
        let mut m = Model::default();
        m.show(ev(1000), t);
        assert_eq!(m.alpha(t), 0.0);
        assert!(m.alpha(t + ms(30)) > 0.5 && m.alpha(t + ms(30)) < 1.0);
        assert_eq!(m.alpha(t + ms(60)), 1.0);
        assert_eq!(m.alpha(t + ms(500)), 1.0);
        assert!(m.alpha(t + ms(950)) < 1.0);
        assert_eq!(m.alpha(t + ms(1000)), 0.0);
    }

    #[test]
    fn a_new_event_while_visible_does_not_fade_in_again() {
        let t = Instant::now();
        let mut m = Model::default();
        m.show(ev(1000), t);
        m.show(ev(1000), t + ms(500));
        assert_eq!(m.alpha(t + ms(500)), 1.0);
        // and the deadline moved
        assert!(!m.expire(t + ms(1200)));
        assert!(m.expire(t + ms(1500)));
    }

    #[test]
    fn an_event_after_expiry_fades_in_again() {
        let t = Instant::now();
        let mut m = Model::default();
        m.show(ev(500), t);
        assert!(m.expire(t + ms(500)));
        m.show(ev(500), t + ms(900));
        assert_eq!(m.alpha(t + ms(900)), 0.0);
    }

    #[test]
    fn wake_is_a_frame_during_fades_and_long_in_between() {
        let t = Instant::now();
        let mut m = Model::default();
        m.show(ev(2000), t);
        assert_eq!(m.next_wake(t), Some(FRAME));
        // holding: sleep until the fade-out begins
        assert_eq!(m.next_wake(t + ms(500)), Some(ms(1380)));
        assert_eq!(m.next_wake(t + ms(1900)), Some(FRAME));
        // never past the expiry
        assert_eq!(m.next_wake(t + ms(1995)), Some(ms(5)));
    }

    #[test]
    fn minimum_timeout_still_shows_something() {
        let t = Instant::now();
        let mut m = Model::default();
        m.show(ev(300), t);
        let peak = (0..30).map(|i| m.alpha(t + ms(i * 10))).fold(0.0, f32::max);
        assert!(peak > 0.9, "peak alpha {peak}");
    }
}
