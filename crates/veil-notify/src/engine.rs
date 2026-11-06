//! The notification logic, with no I/O: what happens to a notification when it
//! arrives, expires, is clicked or is closed, and what the outside world must be
//! told (signals, the bell state, the panel). The clock is always an argument.
//!
//! The app owns the sockets and surfaces; it feeds events in, drains
//! [`Effect`]s out, and draws [`Core::visible`]. Rules:
//!
//! - Rate limit first: a suppressed notification is dropped entirely, and when
//!   its app calms down one "n notifications from X were hidden" entry lands in
//!   the history.
//! - DND hides toasts but keeps history. Critical bypasses DND when
//!   `dnd_critical` is set. A transient notification under DND vanishes.
//! - Transient notifications are shown but never stored.
//! - Expiry sends reason 1 and leaves the entry in the history, unread.
//!   Dismissing (close box, right click) sends reason 2 and marks it read.
//!   `CloseNotification` from the app sends reason 3 and removes it from the
//!   history too: the app says it is no longer relevant.
//! - An action sends `ActionInvoked`, marks the entry read, and closes the toast
//!   unless the notification is `resident`.
//! - Opening the history panel marks everything read.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use veil_ipc::notify::{Control, DndMode, NotificationsState, PanelMode};

use crate::dbus::CloseReason;
use crate::ids::IdGen;
use crate::model::{Limits, Notification, Urgency, DEFAULT_ACTION};
use crate::rate::{self, RateConfig, RateLimiter, Verdict};
use crate::store::Store;
use crate::timers::{TimerConfig, ToastTimers};

/// Changes are written this long after the first one, so a burst is one write.
pub const SAVE_DELAY: Duration = Duration::from_millis(250);

/// Something the outside world has to be told.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    /// `NotificationClosed` on the bus.
    Closed { id: u32, reason: CloseReason },
    /// `ActionInvoked` on the bus.
    Action { id: u32, key: String },
    /// The bell changed; publish it over the Veil IPC.
    Publish(NotificationsState),
    /// The history panel should be open (true) or closed (false).
    Panel(bool),
}

#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub limits: Limits,
    pub timers: TimerConfig,
    pub rate: RateConfig,
    /// Critical notifications are shown even under do-not-disturb.
    pub dnd_critical: bool,
}

impl Default for CoreConfig {
    fn default() -> Self {
        CoreConfig { limits: Limits::default(), timers: TimerConfig::default(), rate: RateConfig::default(), dnd_critical: true }
    }
}

pub struct Core {
    cfg: CoreConfig,
    ids: IdGen,
    store: Store,
    timers: ToastTimers,
    rate: RateLimiter,
    /// The notifications currently on screen (or waiting off screen), by id.
    toasts: HashMap<u32, Notification>,
    published: Option<NotificationsState>,
    panel_open: bool,
    save_due: Option<Instant>,
    rate_check: Option<Instant>,
    effects: Vec<Effect>,
}

impl Core {
    /// Start from a loaded history. The restored bell state is published
    /// straight away so a restarted bar is right at once.
    pub fn new(cfg: CoreConfig, ids: IdGen, store: Store, now: Instant) -> Core {
        let mut core = Core {
            timers: ToastTimers::new(cfg.timers, now),
            rate: RateLimiter::new(cfg.rate),
            cfg,
            ids,
            store,
            toasts: HashMap::new(),
            published: None,
            panel_open: false,
            save_due: None,
            rate_check: None,
            effects: Vec::new(),
        };
        core.publish_if_changed();
        core
    }

    /// Apply the `notify` settings from a config (re)load to a running core.
    /// Toasts already showing keep their timers; the history size and the
    /// rate limit are fixed for the life of the process.
    pub fn set_config(&mut self, timers: TimerConfig, dnd_critical: bool, now: Instant) {
        self.cfg.timers = timers;
        self.cfg.dnd_critical = dnd_critical;
        self.timers.set_config(timers, now);
    }

    // ----- what the app reads

    /// Toasts to show, newest first (top to bottom).
    pub fn visible(&self) -> Vec<&Notification> {
        self.timers.visible_ids().iter().filter_map(|id| self.toasts.get(id)).collect()
    }

    /// Toasts waiting off screen because of the visible cap.
    pub fn hidden_count(&self) -> usize {
        self.timers.hidden_count()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn panel_open(&self) -> bool {
        self.panel_open
    }

    /// Changes whenever the history or DND does.
    pub fn history_rev(&self) -> u64 {
        self.store.rev()
    }

    /// Everything the outside world must be told since the last call.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// When the app must call [`Core::on_tick`] next; `None` means sleep until something happens.
    pub fn next_wake(&self) -> Option<Instant> {
        [self.timers.next_deadline(), self.save_due, self.rate_check.filter(|_| self.rate.has_pending())]
            .into_iter()
            .flatten()
            .min()
    }

    // ----- what happens

    /// A notification arrived from an app. `replaced` means it reuses an id.
    pub fn on_notify(&mut self, n: Notification, replaced: bool, now: Instant) {
        let key = rate::key(&n.app_name, n.desktop_entry.as_deref());
        match self.rate.check(&key, now) {
            Verdict::Suppress => {
                self.rate_check = Some(now + self.cfg.rate.window);
                return self.after(now);
            }
            Verdict::Allow { suppressed_before } if suppressed_before > 0 => {
                self.push_suppressed_summary(&n.app_name, suppressed_before, n.created_ms);
            }
            Verdict::Allow { .. } => {}
        }

        let shows = !self.store.dnd() || (n.urgency == Urgency::Critical && self.cfg.dnd_critical);
        if n.transient {
            // Never stored; and it must not leave a stale stored copy of the id behind.
            if replaced {
                self.store.remove(n.id);
            }
        } else {
            self.store.push(n.clone());
        }
        if shows {
            self.timers.add(n.id, n.urgency, n.timeout_ms, now);
            self.toasts.insert(n.id, n);
        } else if self.timers.remove(n.id, now) {
            // Replacing a live toast with one DND hides: the old one goes quietly.
            self.toasts.remove(&n.id);
        }
        self.collect_expired(now);
        self.after(now);
    }

    /// The app called `CloseNotification`: gone from the screen and the history.
    pub fn on_close_request(&mut self, id: u32, now: Instant) {
        if self.timers.remove(id, now) {
            self.toasts.remove(&id);
            self.effects.push(Effect::Closed { id, reason: CloseReason::Closed });
        }
        self.store.remove(id);
        self.collect_expired(now);
        self.after(now);
    }

    /// The user closed a toast (close box or right click).
    pub fn dismiss(&mut self, id: u32, now: Instant) {
        self.close_toast(id, CloseReason::Dismissed, now);
    }

    /// The user clicked an action button.
    pub fn activate(&mut self, id: u32, key: &str, now: Instant) {
        let Some(n) = self.toasts.get(&id) else { return };
        if !n.actions.iter().any(|a| a.key == key) {
            return;
        }
        let resident = n.resident;
        self.effects.push(Effect::Action { id, key: key.to_string() });
        self.store.mark_read(id);
        if !resident {
            self.close_toast(id, CloseReason::Dismissed, now);
        } else {
            self.after(now);
        }
    }

    /// The user clicked the toast itself: its default action if it has one, and dismiss.
    pub fn body_click(&mut self, id: u32, now: Instant) {
        match self.toasts.get(&id).map(|n| n.default_action().is_some()) {
            Some(true) => self.activate(id, DEFAULT_ACTION, now),
            Some(false) => self.dismiss(id, now),
            None => {}
        }
    }

    /// Remove one entry from the history (the panel's row button).
    pub fn remove_history(&mut self, id: u32, now: Instant) {
        self.store.remove(id);
        self.after(now);
    }

    /// Empty the history (the panel's Clear). Toasts on screen are left alone.
    pub fn clear_history(&mut self, now: Instant) {
        self.store.clear();
        self.after(now);
    }

    /// The pointer entered or left the toasts.
    pub fn hover(&mut self, on: bool, now: Instant) {
        self.timers.set_hover(on, now);
        self.collect_expired(now);
        self.after(now);
    }

    /// `notify.dnd` / `notify.panel` from the master.
    pub fn on_control(&mut self, c: Control, now: Instant) {
        if let Some(mode) = c.dnd {
            let on = match mode {
                DndMode::On => true,
                DndMode::Off => false,
                DndMode::Toggle => !self.store.dnd(),
            };
            self.store.set_dnd(on);
        }
        if let Some(mode) = c.panel {
            let open = match mode {
                PanelMode::Open => true,
                PanelMode::Close => false,
                PanelMode::Toggle => !self.panel_open,
            };
            if open != self.panel_open {
                self.panel_open = open;
                self.effects.push(Effect::Panel(open));
                if open {
                    self.store.mark_all_read();
                }
            }
        }
        self.after(now);
    }

    /// Time passed: expire toasts, report quiet rate-limited apps.
    pub fn on_tick(&mut self, now: Instant, wall_ms: u64) {
        self.collect_expired(now);
        if self.rate_check.is_some_and(|t| t <= now) {
            for (app, count) in self.rate.flush_idle(now) {
                self.push_suppressed_summary(&app, count, wall_ms);
            }
            self.rate_check = self.rate.has_pending().then(|| now + self.cfg.rate.window);
        }
        self.after(now);
    }

    /// Write the history if the debounce has run out. `None` when nothing was due.
    pub fn save_if_due(&mut self, now: Instant, path: &Path) -> Option<io::Result<()>> {
        if self.save_due.is_some_and(|t| t <= now) {
            self.save_due = None;
            self.store.take_dirty();
            return Some(self.store.save(path, self.ids.peek()));
        }
        None
    }

    /// Write the history now (shutdown).
    pub fn save_now(&mut self, path: &Path) -> io::Result<()> {
        self.save_due = None;
        self.store.take_dirty();
        self.store.save(path, self.ids.peek())
    }

    // ----- internals

    fn close_toast(&mut self, id: u32, reason: CloseReason, now: Instant) {
        if self.timers.remove(id, now) {
            self.toasts.remove(&id);
            self.store.mark_read(id);
            self.effects.push(Effect::Closed { id, reason });
        }
        self.collect_expired(now);
        self.after(now);
    }

    fn collect_expired(&mut self, now: Instant) {
        for id in self.timers.poll(now) {
            self.toasts.remove(&id);
            self.effects.push(Effect::Closed { id, reason: CloseReason::Expired });
        }
    }

    fn push_suppressed_summary(&mut self, app: &str, count: u32, wall_ms: u64) {
        let note = Notification {
            id: self.ids.alloc(),
            app_name: "veil".into(),
            summary: format!("{count} notifications from {app} were hidden"),
            body: String::new(),
            actions: Vec::new(),
            urgency: Urgency::Low,
            transient: false,
            resident: false,
            desktop_entry: None,
            icon: None,
            image: None,
            timeout_ms: -1,
            created_ms: wall_ms,
            read: false,
        };
        self.store.push(note);
    }

    fn publish_if_changed(&mut self) {
        let now_state = self.store.summary();
        let state = NotificationsState { count: now_state.count, dnd: now_state.dnd };
        if self.published != Some(state) {
            self.published = Some(state);
            self.effects.push(Effect::Publish(state));
        }
    }

    fn after(&mut self, now: Instant) {
        self.publish_if_changed();
        if self.store.is_dirty() && self.save_due.is_none() {
            self.save_due = Some(now + SAVE_DELAY);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Hints, NotifyArgs};

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn core(t0: Instant) -> Core {
        Core::new(CoreConfig::default(), IdGen::new(1), Store::new(100), t0)
    }

    struct N {
        id: u32,
        app: &'static str,
        summary: &'static str,
        urgency: Option<u8>,
        timeout: i32,
        transient: bool,
        resident: bool,
        actions: &'static [&'static str],
    }

    fn n(id: u32) -> N {
        N { id, app: "app", summary: "hello", urgency: None, timeout: -1, transient: false, resident: false, actions: &[] }
    }

    impl N {
        fn build(self) -> Notification {
            Notification::from_args(
                self.id,
                NotifyArgs {
                    app_name: self.app.into(),
                    summary: self.summary.into(),
                    actions: self.actions.iter().map(|s| s.to_string()).collect(),
                    expire_timeout: self.timeout,
                    hints: Hints { urgency: self.urgency, transient: self.transient, resident: self.resident, ..Hints::default() },
                    ..NotifyArgs::default()
                },
                &Limits::default(),
                1000,
            )
        }
    }

    fn ids(c: &Core) -> Vec<u32> {
        c.visible().iter().map(|n| n.id).collect()
    }

    fn state(count: u32, dnd: bool) -> Effect {
        Effect::Publish(NotificationsState { count, dnd })
    }

    #[test]
    fn a_new_notification_is_shown_stored_and_counted() {
        let t0 = Instant::now();
        let mut c = core(t0);
        assert_eq!(c.take_effects(), vec![state(0, false)], "the initial bell state");
        c.on_notify(n(1).build(), false, t0);
        assert_eq!(ids(&c), vec![1]);
        assert_eq!(c.store().len(), 1);
        assert_eq!(c.take_effects(), vec![state(1, false)]);
    }

    #[test]
    fn expiry_closes_the_toast_but_keeps_the_history_unread() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.take_effects();
        c.on_notify(n(1).build(), false, t0);
        c.take_effects();
        c.on_tick(at(t0, 4999), 0);
        assert_eq!(ids(&c), vec![1]);
        c.on_tick(at(t0, 5000), 0);
        assert!(ids(&c).is_empty());
        assert_eq!(c.take_effects(), vec![Effect::Closed { id: 1, reason: CloseReason::Expired }]);
        assert_eq!(c.store().unread(), 1, "an unseen notification stays unread");
    }

    #[test]
    fn dnd_hides_toasts_keeps_history_and_critical_bypasses() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_control(Control { dnd: Some(DndMode::On), panel: None }, t0);
        c.take_effects();
        c.on_notify(n(1).build(), false, t0);
        assert!(ids(&c).is_empty(), "no toast under DND");
        assert_eq!(c.store().len(), 1, "but it is in the history");
        c.on_notify(N { urgency: Some(2), ..n(2) }.build(), false, t0);
        assert_eq!(ids(&c), vec![2], "critical bypasses");
        c.on_notify(N { transient: true, ..n(3) }.build(), false, t0);
        assert_eq!(c.store().len(), 2, "a transient one under DND vanishes without a trace");
        assert!(!ids(&c).contains(&3));
        assert_eq!(c.take_effects().last(), Some(&state(2, true)));
    }

    #[test]
    fn dnd_critical_can_be_turned_off() {
        let t0 = Instant::now();
        let cfg = CoreConfig { dnd_critical: false, ..CoreConfig::default() };
        let mut c = Core::new(cfg, IdGen::new(1), Store::new(10), t0);
        c.on_control(Control { dnd: Some(DndMode::On), panel: None }, t0);
        c.on_notify(N { urgency: Some(2), ..n(1) }.build(), false, t0);
        assert!(ids(&c).is_empty());
    }

    #[test]
    fn transient_is_shown_but_never_stored() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(N { transient: true, ..n(1) }.build(), false, t0);
        assert_eq!(ids(&c), vec![1]);
        assert!(c.store().is_empty());
        assert_eq!(c.store().unread(), 0);
    }

    #[test]
    fn replacing_swaps_in_place_and_restarts_the_timer() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(n(1).build(), false, t0);
        c.on_notify(N { summary: "second", ..n(1) }.build(), true, at(t0, 3000));
        assert_eq!(c.visible().len(), 1);
        assert_eq!(c.visible()[0].summary, "second");
        assert_eq!(c.store().len(), 1);
        assert_eq!(c.store().get(1).unwrap().summary, "second");
        c.on_tick(at(t0, 5000), 0);
        assert_eq!(ids(&c), vec![1], "the timer restarted at 3 s");
        c.on_tick(at(t0, 8000), 0);
        assert!(ids(&c).is_empty());
    }

    #[test]
    fn replacing_a_live_toast_with_one_dnd_hides_removes_it_quietly() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(n(1).build(), false, t0);
        c.take_effects();
        c.on_control(Control { dnd: Some(DndMode::On), panel: None }, t0);
        c.on_notify(N { summary: "again", ..n(1) }.build(), true, t0);
        assert!(ids(&c).is_empty());
        assert!(!c.take_effects().iter().any(|e| matches!(e, Effect::Closed { .. })), "a replacement is not a close");
    }

    #[test]
    fn a_flooding_app_is_cut_off_and_summarised_once_it_calms_down() {
        let t0 = Instant::now();
        let cfg = CoreConfig { rate: RateConfig { max: 2, window: Duration::from_secs(10) }, ..CoreConfig::default() };
        let mut c = Core::new(cfg, IdGen::new(100), Store::new(100), t0);
        for id in 1..=5 {
            c.on_notify(N { app: "spam", ..n(id) }.build(), false, at(t0, id as u64));
        }
        assert_eq!(c.visible().len(), 2, "only the first two get through");
        assert_eq!(c.store().len(), 2);
        c.on_notify(N { app: "other", ..n(9) }.build(), false, at(t0, 10));
        assert_eq!(c.visible().len(), 3, "another app is unaffected");
        assert!(c.next_wake().is_some());

        c.on_tick(at(t0, 30_000), 5555);
        let summary = c.store().newest_first().next().unwrap();
        assert_eq!(summary.summary, "3 notifications from spam were hidden");
        assert_eq!((summary.urgency, summary.app_name.as_str(), summary.created_ms), (Urgency::Low, "veil", 5555));
        c.on_tick(at(t0, 60_000), 6000);
        assert_eq!(c.store().newest_first().filter(|n| n.summary.contains("hidden")).count(), 1, "reported once");
    }

    #[test]
    fn dismissing_closes_the_toast_marks_it_read_and_updates_the_bell() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(n(1).build(), false, t0);
        c.take_effects();
        c.dismiss(1, t0);
        assert!(ids(&c).is_empty());
        assert_eq!(c.store().unread(), 0);
        assert_eq!(c.store().len(), 1, "it stays in the history");
        assert_eq!(c.take_effects(), vec![Effect::Closed { id: 1, reason: CloseReason::Dismissed }, state(0, false)]);
        c.dismiss(1, t0);
        assert!(c.take_effects().is_empty(), "dismissing twice does nothing");
    }

    #[test]
    fn an_action_is_reported_and_closes_the_toast_unless_resident() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(N { actions: &["yes", "Yes", "no", "No"], ..n(1) }.build(), false, t0);
        c.on_notify(N { actions: &["open", "Open"], resident: true, ..n(2) }.build(), false, t0);
        c.take_effects();

        c.activate(1, "bogus", t0);
        c.activate(77, "yes", t0);
        assert!(c.take_effects().is_empty(), "an unknown key or id is ignored");

        c.activate(1, "yes", t0);
        assert_eq!(
            c.take_effects(),
            vec![Effect::Action { id: 1, key: "yes".into() }, Effect::Closed { id: 1, reason: CloseReason::Dismissed }, state(1, false)]
        );
        c.activate(2, "open", t0);
        assert_eq!(ids(&c), vec![2], "a resident notification stays after its action");
        assert!(c.take_effects().contains(&Effect::Action { id: 2, key: "open".into() }));
    }

    #[test]
    fn clicking_the_body_runs_the_default_action_or_just_dismisses() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(N { actions: &["default", "Open"], ..n(1) }.build(), false, t0);
        c.on_notify(n(2).build(), false, t0);
        c.take_effects();
        c.body_click(1, t0);
        assert!(c.take_effects().contains(&Effect::Action { id: 1, key: "default".into() }));
        c.body_click(2, t0);
        assert!(c.take_effects().contains(&Effect::Closed { id: 2, reason: CloseReason::Dismissed }));
        c.body_click(99, t0);
        assert!(c.take_effects().is_empty());
    }

    #[test]
    fn an_app_closing_its_notification_removes_it_everywhere() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(n(1).build(), false, t0);
        c.on_notify(n(2).build(), false, t0);
        c.take_effects();
        c.on_close_request(1, t0);
        assert_eq!(ids(&c), vec![2]);
        assert!(c.store().get(1).is_none());
        assert!(c.take_effects().contains(&Effect::Closed { id: 1, reason: CloseReason::Closed }));
        // one that already expired off the screen: gone from the history, no second signal
        c.on_tick(at(t0, 6000), 0);
        c.take_effects();
        c.on_close_request(2, at(t0, 6000));
        assert!(c.store().get(2).is_none());
        assert!(!c.take_effects().iter().any(|e| matches!(e, Effect::Closed { .. })));
    }

    #[test]
    fn dnd_and_panel_controls_publish_and_resolve_toggles() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(n(1).build(), false, t0);
        c.take_effects();
        let toggle = Control { dnd: Some(DndMode::Toggle), panel: None };
        c.on_control(toggle, t0);
        assert_eq!(c.take_effects(), vec![state(1, true)]);
        c.on_control(toggle, t0);
        assert_eq!(c.take_effects(), vec![state(1, false)]);
        c.on_control(Control { dnd: Some(DndMode::Off), panel: None }, t0);
        assert!(c.take_effects().is_empty(), "no change, nothing to publish");

        c.on_control(Control { dnd: None, panel: Some(PanelMode::Toggle) }, t0);
        assert_eq!(c.take_effects(), vec![Effect::Panel(true), state(0, false)], "opening reads everything");
        assert!(c.panel_open());
        c.on_control(Control { dnd: None, panel: Some(PanelMode::Open) }, t0);
        assert!(c.take_effects().is_empty(), "already open");
        c.on_control(Control { dnd: None, panel: Some(PanelMode::Close) }, t0);
        assert_eq!(c.take_effects(), vec![Effect::Panel(false)]);
    }

    #[test]
    fn hovering_pauses_every_toast() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(n(1).build(), false, t0);
        c.hover(true, at(t0, 2000));
        c.on_tick(at(t0, 20_000), 0);
        assert_eq!(ids(&c), vec![1]);
        // the debounced save is still owed; once it is written nothing is left to wake for
        let dir = tempfile::tempdir().unwrap();
        c.save_if_due(at(t0, 20_000), &dir.path().join("h.json")).unwrap().unwrap();
        assert_eq!(c.next_wake(), None, "paused: nothing to wake for");
        c.hover(false, at(t0, 20_000));
        c.on_tick(at(t0, 22_999), 0);
        assert_eq!(ids(&c), vec![1]);
        c.on_tick(at(t0, 23_000), 0);
        assert!(ids(&c).is_empty());
    }

    #[test]
    fn at_most_five_toasts_are_visible_and_the_rest_wait() {
        let t0 = Instant::now();
        let mut c = core(t0);
        for id in 1..=7 {
            c.on_notify(n(id).build(), false, t0);
        }
        assert_eq!(ids(&c), vec![7, 6, 5, 4, 3], "newest on top");
        assert_eq!(c.hidden_count(), 2);
        assert_eq!(c.store().len(), 7);
    }

    #[test]
    fn the_bell_is_published_only_when_it_changes() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.take_effects();
        c.on_notify(n(1).build(), false, t0);
        c.on_notify(N { summary: "edit", ..n(1) }.build(), true, t0);
        let publishes: Vec<_> = c.take_effects().into_iter().filter(|e| matches!(e, Effect::Publish(_))).collect();
        assert_eq!(publishes, vec![state(1, false)], "an edit that leaves the count alone publishes nothing");
    }

    #[test]
    fn a_restart_publishes_the_restored_bell_and_continues_the_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        let t0 = Instant::now();
        let ids_a = IdGen::new(1);
        let mut c = Core::new(CoreConfig::default(), ids_a.clone(), Store::new(100), t0);
        for _ in 0..3 {
            let id = ids_a.alloc();
            c.on_notify(n(id).build(), false, t0);
        }
        c.on_control(Control { dnd: Some(DndMode::On), panel: None }, t0);
        c.save_now(&path).unwrap();

        let loaded = Store::load(&path, &Limits::default());
        assert_eq!(loaded.next_id, 4);
        let ids_b = IdGen::new(loaded.next_id);
        let mut c2 = Core::new(CoreConfig::default(), ids_b.clone(), loaded.store, t0);
        assert_eq!(c2.take_effects(), vec![state(3, true)], "bell and DND survive the restart");
        assert!(ids(&c2).is_empty(), "restored entries are history only, never re-toasted");
        assert_eq!(ids_b.alloc(), 4);
    }

    #[test]
    fn saves_are_debounced_into_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        let t0 = Instant::now();
        let mut c = core(t0);
        assert_eq!(c.next_wake(), None, "idle means no wake-up at all");
        c.on_notify(n(1).build(), false, t0);
        c.on_notify(n(2).build(), false, at(t0, 100));
        assert!(c.save_if_due(at(t0, 200), &path).is_none(), "not yet");
        assert!(!path.exists());
        let due = c.next_wake().unwrap();
        assert!(due <= at(t0, 250) && due >= at(t0, 5), "the first change started the clock");
        assert!(c.save_if_due(at(t0, 250), &path).unwrap().is_ok());
        assert_eq!(Store::load(&path, &Limits::default()).store.len(), 2, "both changes in one write");
        assert!(c.save_if_due(at(t0, 900), &path).is_none(), "nothing changed since");
    }

    #[test]
    fn next_wake_is_the_soonest_of_timer_and_save() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(N { timeout: 800, ..n(1) }.build(), false, t0);
        assert_eq!(c.next_wake(), Some(at(t0, 250)), "the save is due before the toast expires");
        let dir = tempfile::tempdir().unwrap();
        c.save_if_due(at(t0, 250), &dir.path().join("h.json")).unwrap().unwrap();
        assert_eq!(c.next_wake(), Some(at(t0, 800)));
    }

    #[test]
    fn expiry_found_during_another_event_still_reports_the_close() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(n(1).build(), false, t0);
        c.take_effects();
        c.on_notify(n(2).build(), false, at(t0, 6000));
        assert!(c.take_effects().contains(&Effect::Closed { id: 1, reason: CloseReason::Expired }));
        assert_eq!(ids(&c), vec![2]);
    }

    #[test]
    fn set_config_changes_the_default_timeout_for_later_toasts() {
        let t0 = Instant::now();
        let mut c = core(t0);
        let timers = TimerConfig { default_timeout: Some(Duration::from_secs(2)), ..TimerConfig::default() };
        c.set_config(timers, true, t0);
        c.on_notify(n(1).build(), false, t0);
        c.on_tick(at(t0, 1999), 0);
        assert_eq!(ids(&c), vec![1]);
        c.on_tick(at(t0, 2000), 0);
        assert!(ids(&c).is_empty());
    }

    #[test]
    fn set_config_none_keeps_toasts_until_dismissed() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.set_config(TimerConfig { default_timeout: None, ..TimerConfig::default() }, true, t0);
        c.on_notify(n(1).build(), false, t0);
        c.on_tick(at(t0, 3_600_000), 0);
        assert_eq!(ids(&c), vec![1]);
    }

    #[test]
    fn set_config_keeps_the_time_a_showing_toast_already_used() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_notify(n(1).build(), false, t0); // 5 s default
        c.set_config(TimerConfig { default_timeout: Some(Duration::from_secs(60)), ..TimerConfig::default() }, true, at(t0, 4000));
        c.on_tick(at(t0, 5000), 0);
        assert!(ids(&c).is_empty(), "the toast already running still ends on its original 5 s");
    }

    #[test]
    fn set_config_lowers_the_visible_cap() {
        let t0 = Instant::now();
        let mut c = core(t0);
        for id in 1..=3 {
            c.on_notify(n(id).build(), false, t0);
        }
        assert_eq!(ids(&c), vec![3, 2, 1]);
        c.set_config(TimerConfig { max_visible: 1, ..TimerConfig::default() }, true, t0);
        assert_eq!(ids(&c), vec![3]);
        assert_eq!(c.hidden_count(), 2);
    }

    #[test]
    fn set_config_can_stop_critical_bypassing_dnd() {
        let t0 = Instant::now();
        let mut c = core(t0);
        c.on_control(Control { dnd: Some(DndMode::On), panel: None }, t0);
        c.on_notify(N { urgency: Some(2), ..n(1) }.build(), false, t0);
        assert_eq!(ids(&c), vec![1], "critical passes under DND by default");
        c.set_config(TimerConfig::default(), false, t0);
        c.on_notify(N { urgency: Some(2), ..n(2) }.build(), false, t0);
        assert_eq!(ids(&c), vec![1], "the second critical is now held back");
    }
}
