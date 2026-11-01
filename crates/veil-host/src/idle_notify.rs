//! `ext-idle-notify-v1` (version 2).
//!
//! Smithay's own `IdleNotifierState<D>` wants a `LoopHandle<'static, D>`, but
//! Veil's event loop carries `LoopData` (State + Display), so it can't be
//! used as-is. This is the same behaviour on Veil's loop, with one change in
//! shape: Smithay re-arms a calloop timer per notification on every input
//! event; here input only stamps `last_activity` (one clock read), and a
//! single timer fires at the earliest deadline, re-checking each
//! notification against the *current* `last_activity` when it does.
//!
//! Veil has exactly one seat, so the `wl_seat` argument is accepted and
//! ignored: any input activity counts for every notification.
//!
//! Idle inhibit is tracked here too (`inhibit`/`uninhibit` were no-ops):
//! while any inhibitor surface is alive, `get_idle_notification` listeners
//! never go idle; `get_input_idle_notification` listeners ignore inhibitors,
//! as the protocol specifies.

use std::time::{Duration, Instant};

use calloop::{
    timer::{TimeoutAction, Timer},
    LoopHandle, RegistrationToken,
};
use smithay::reexports::wayland_protocols::ext::idle_notify::v1::server::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
};
use smithay::reexports::wayland_server::{
    protocol::wl_surface::WlSurface, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch,
    New, Resource,
};

use crate::server::{LoopData, State};

type Lh<'a> = Option<&'a LoopHandle<'static, LoopData>>;

struct Notif {
    res: ExtIdleNotificationV1,
    timeout: Duration,
    /// When the client asked; idle time never counts from before this
    /// (otherwise a fresh 5-minute listener would fire instantly on a
    /// machine nobody has touched since boot).
    since: Instant,
    idle: bool,
    /// `get_input_idle_notification`: not suppressed by idle inhibitors.
    ignore_inhibit: bool,
}

pub struct IdleNotify {
    notifs: Vec<Notif>,
    last_activity: Instant,
    inhibitors: Vec<WlSurface>,
    armed: Option<RegistrationToken>,
}

impl IdleNotify {
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<State, ExtIdleNotifierV1, ()>(2, ());
        Self {
            notifs: Vec::new(),
            last_activity: Instant::now(),
            inhibitors: Vec::new(),
            armed: None,
        }
    }

    fn inhibited(&mut self) -> bool {
        self.inhibitors.retain(|s| s.is_alive());
        !self.inhibitors.is_empty()
    }

    fn deadline(&self, n: &Notif) -> Instant {
        self.last_activity.max(n.since) + n.timeout
    }

    fn resume_all(&mut self, only_inhibitable: bool) {
        for n in &mut self.notifs {
            if n.idle && !(only_inhibitable && n.ignore_inhibit) {
                n.res.resumed();
                n.idle = false;
            }
        }
    }

    /// Call on every real input event (key, pointer, button, scroll).
    pub fn activity(&mut self, lh: Lh) {
        self.last_activity = Instant::now();
        if self.notifs.is_empty() {
            return;
        }
        let any_idle = self.notifs.iter().any(|n| n.idle);
        if any_idle {
            self.resume_all(false);
        }
        // Hot path: the common case is "timer already armed, nobody idle".
        if any_idle || self.armed.is_none() {
            self.arm(lh);
        }
    }

    pub fn inhibit(&mut self, surface: WlSurface, lh: Lh) {
        let was_inhibited = self.inhibited();
        if !self.inhibitors.contains(&surface) {
            self.inhibitors.push(surface);
        }
        if !was_inhibited {
            self.resume_all(true);
        }
        let _ = lh;
    }

    pub fn uninhibit(&mut self, surface: &WlSurface, lh: Lh) {
        self.inhibitors.retain(|s| s != surface);
        if !self.inhibited() {
            // Idle time restarts when the inhibitor goes away.
            self.last_activity = Instant::now();
            self.arm(lh);
        }
    }

    /// Fire everything whose deadline has passed.
    fn tick(&mut self) {
        let inhibited = self.inhibited();
        let now = Instant::now();
        let last = self.last_activity;
        for n in &mut self.notifs {
            if n.idle || (inhibited && !n.ignore_inhibit) {
                continue;
            }
            if now >= last.max(n.since) + n.timeout {
                n.res.idled();
                n.idle = true;
            }
        }
    }

    /// Arm one timer at the earliest pending deadline (no-op if armed or
    /// nothing is pending).
    fn arm(&mut self, lh: Lh) {
        if self.armed.is_some() {
            return;
        }
        let Some(lh) = lh else { return };
        let inhibited = self.inhibited();
        let next = self
            .notifs
            .iter()
            .filter(|n| !n.idle && (!inhibited || n.ignore_inhibit))
            .map(|n| self.deadline(n))
            .min();
        let Some(next) = next else { return };
        let wait = next.saturating_duration_since(Instant::now());
        self.armed = lh
            .insert_source(Timer::from_duration(wait), |_, _, data: &mut LoopData| {
                let lh = data.state.loop_handle.clone();
                data.state.idle_notify.armed = None;
                data.state.idle_notify.tick();
                data.state.idle_notify.arm(lh.as_ref());
                TimeoutAction::Drop
            })
            .ok();
    }
}

impl GlobalDispatch<ExtIdleNotifierV1, (), State> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtIdleNotifierV1>,
        _: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ExtIdleNotifierV1, (), State> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        _res: &ExtIdleNotifierV1,
        request: ext_idle_notifier_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        let (id, timeout, ignore_inhibit) = match request {
            ext_idle_notifier_v1::Request::GetIdleNotification { id, timeout, .. } => {
                (id, timeout, false)
            }
            ext_idle_notifier_v1::Request::GetInputIdleNotification { id, timeout, .. } => {
                (id, timeout, true)
            }
            _ => return,
        };
        let res = data_init.init(id, ());
        state.idle_notify.notifs.push(Notif {
            res,
            timeout: Duration::from_millis(timeout as u64),
            since: Instant::now(),
            idle: false,
            ignore_inhibit,
        });
        let lh = state.loop_handle.clone();
        state.idle_notify.arm(lh.as_ref());
    }
}

impl Dispatch<ExtIdleNotificationV1, (), State> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _res: &ExtIdleNotificationV1,
        _request: ext_idle_notification_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
    }

    fn destroyed(
        state: &mut State,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        res: &ExtIdleNotificationV1,
        _: &(),
    ) {
        state.idle_notify.notifs.retain(|n| &n.res != res);
    }
}
