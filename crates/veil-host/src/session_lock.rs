//! Host side of `ext-session-lock-v1`: the real session lock.
//!
//! The lock screen itself is a client (`veil shell lock`). What makes it a
//! *lock* is this module: while it is active the desktop is not composited,
//! no keybind runs, and the keyboard goes only to a lock surface. The lock
//! holds even if the client dies.
//!
//! Phases:
//!
//! - `Unlocked`: normal operation.
//! - `Cover`: the host asked for a lock (idle, lid, power menu) and no lock
//!   client has taken it yet. Screen dimmed, input blocked, client being
//!   started. Fail-closed: if the client never arrives, the session stays
//!   covered rather than quietly showing the desktop.
//! - `Locking`: a client sent `lock`. The next composited frame shows no
//!   desktop on any output, then `locked` is confirmed.
//! - `Locked`: confirmed. Only `unlock_and_destroy` from the SAME lock object
//!   ends it. If that client dies the phase stays `Locked`, the host
//!   respawns the client, and a new `lock` request takes over.
//!
//! Two gaps in Smithay 0.7.0 are handled here:
//!
//! 1. `unlock_and_destroy` on a lock that was never confirmed posts a
//!    protocol error but STILL calls `SessionLockHandler::unlock`. Any
//!    client could unlock by sending `lock` while locked (refused with
//!    `finished`) and then `unlock_and_destroy`. [`Dispatch`] for
//!    `ExtSessionLockV1` is therefore ours, and rejects an unlock that does
//!    not come from the holder before Smithay sees it.
//! 2. Smithay never forgets which outputs were locked when a client dies, so
//!    the replacement hits `duplicate_output`. [`reset_manager`] swaps in a
//!    fresh manager state; the superseded global is hidden by its filter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::{
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    ext_session_lock_surface_v1::ExtSessionLockSurfaceV1,
    ext_session_lock_v1::{self, ExtSessionLockV1},
};
use smithay::reexports::wayland_server::{
    delegate_dispatch, delegate_global_dispatch,
    protocol::{wl_output::WlOutput, wl_surface::WlSurface},
    Client, DataInit, Dispatch, DisplayHandle, Resource,
};
use smithay::wayland::session_lock::{
    ExtLockSurfaceUserData, LockSurface, SessionLockHandler, SessionLockManagerGlobalData,
    SessionLockManagerState, SessionLockState, SessionLocker,
};
use veil_text::Role;

use crate::layout::Rect;
use crate::server::{self, State};
use crate::supervisor::SupervisorState;

/// Minimum gap between attempts to start the lock client.
const SPAWN_GAP: Duration = Duration::from_secs(2);
/// A crash-looped (`Failed`) lock client is retried this often: staying
/// covered with no client is a lockout, so the host never gives up for good.
const FAILED_RETRY: Duration = Duration::from_secs(30);
/// Lock + suspend: how long a client may take before suspending anyway. The
/// screen is already covered by then, so waking up shows the cover, not the
/// desktop.
const COVER_SUSPEND_GRACE: Duration = Duration::from_secs(5);

pub enum Phase {
    Unlocked,
    Cover,
    Locking(SessionLocker),
    Locked,
}

struct Entry {
    /// Monitor index the surface is on.
    idx: usize,
    surface: LockSurface,
    /// Size last configured.
    size: (u32, u32),
}

pub struct LockController {
    phase: Phase,
    /// The lock object that holds (or is taking) the lock.
    lock_res: Option<ExtSessionLockV1>,
    surfaces: Vec<Entry>,
    /// Whether the current manager global may be seen by new clients.
    visible: Arc<AtomicBool>,
    cover_since: Option<Instant>,
    last_spawn: Option<Instant>,
    /// Lid / idle `lock+suspend`: suspend once the lock is in place.
    pub suspend_when_locked: bool,
}

impl LockController {
    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Unlocked)
    }

    pub fn is_locked(&self) -> bool {
        matches!(self.phase, Phase::Locked)
    }

    fn client_alive(&self) -> bool {
        self.lock_res.as_ref().is_some_and(|r| r.is_alive())
    }

    fn owns(&self, lock: &ExtSessionLockV1) -> bool {
        self.lock_res.as_ref() == Some(lock)
    }

    fn surface_for(&self, idx: usize) -> Option<&LockSurface> {
        self.surfaces.iter().find(|e| e.idx == idx && e.surface.alive()).map(|e| &e.surface)
    }
}

/// Create the protocol global and the controller. Called once at startup.
pub fn init(dh: &DisplayHandle) -> (SessionLockManagerState, LockController) {
    let visible = Arc::new(AtomicBool::new(true));
    let manager = new_manager(dh, visible.clone());
    let ctl = LockController {
        phase: Phase::Unlocked,
        lock_res: None,
        surfaces: Vec::new(),
        visible,
        cover_since: None,
        last_spawn: None,
        suspend_when_locked: false,
    };
    (manager, ctl)
}

fn new_manager(dh: &DisplayHandle, visible: Arc<AtomicBool>) -> SessionLockManagerState {
    // Any same-user client may lock (third-party lockers work); only the
    // holder of the lock can unlock it.
    SessionLockManagerState::new::<State, _>(dh, move |_client| visible.load(Ordering::Relaxed))
}

/// Forget Smithay's list of locked outputs (see the module docs).
fn reset_manager(state: &mut State) {
    state.lock.visible.store(false, Ordering::Relaxed);
    let visible = Arc::new(AtomicBool::new(true));
    state.lock.visible = visible.clone();
    let dh = state.display_handle.clone();
    state.session_lock_state = new_manager(&dh, visible);
}

// ─── Protocol glue ───────────────────────────────────────────────────────

delegate_global_dispatch!(State: [ExtSessionLockManagerV1: SessionLockManagerGlobalData] => SessionLockManagerState);
delegate_dispatch!(State: [ExtSessionLockManagerV1: ()] => SessionLockManagerState);
delegate_dispatch!(State: [ExtSessionLockSurfaceV1: ExtLockSurfaceUserData] => SessionLockManagerState);

impl Dispatch<ExtSessionLockV1, SessionLockState> for State {
    fn request(
        state: &mut Self,
        client: &Client,
        lock: &ExtSessionLockV1,
        request: ext_session_lock_v1::Request,
        data: &SessionLockState,
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if matches!(request, ext_session_lock_v1::Request::UnlockAndDestroy) && !state.lock.owns(lock) {
            tracing::warn!("session-lock: unlock from a client that does not hold the lock; refused");
            lock.post_error(ext_session_lock_v1::Error::InvalidUnlock, "This client does not hold the lock.");
            return;
        }
        <SessionLockManagerState as Dispatch<ExtSessionLockV1, SessionLockState, State>>::request(
            state, client, lock, request, data, dh, data_init,
        )
    }
}

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_state
    }

    fn lock(&mut self, locker: SessionLocker) {
        on_lock(self, locker);
    }

    fn unlock(&mut self) {
        on_unlock(self);
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        on_new_surface(self, surface, output);
    }
}

fn on_lock(state: &mut State, locker: SessionLocker) {
    match state.lock.phase {
        // Someone else is mid-lock, or a live client already holds it:
        // dropping the locker tells this client `finished`.
        Phase::Locking(_) => return,
        Phase::Locked if state.lock.client_alive() => return,
        // The holder died: this client takes over, and Smithay's stale
        // output list has to go first.
        Phase::Locked => reset_manager(state),
        Phase::Cover | Phase::Unlocked => {}
    }
    tracing::info!("session-lock: lock requested by a client");
    close_modals(state);
    state.lock.lock_res = Some(locker.ext_session_lock().clone());
    state.lock.surfaces.clear();
    state.lock.cover_since = None;
    state.lock.phase = Phase::Locking(locker);
    server::mark_dirty_full(state);
}

fn on_unlock(state: &mut State) {
    tracing::info!("session-lock: unlocked");
    state.lock.phase = Phase::Unlocked;
    state.lock.lock_res = None;
    state.lock.surfaces.clear();
    state.lock.cover_since = None;
    state.lock.suspend_when_locked = false;
    state.idle_locked = false;
    server::refocus_keyboard(state);
    server::mark_dirty_full(state);
}

fn on_new_surface(state: &mut State, surface: LockSurface, output: WlOutput) {
    let idx = Output::from_resource(&output).and_then(|o| state.layer_outputs.iter().position(|x| *x == o));
    let Some(idx) = idx else {
        tracing::warn!("session-lock: lock surface for an unknown output");
        return;
    };
    let size = (state.monitors[idx].output_w, state.monitors[idx].output_h);
    // Smithay sends the first configure right after this returns.
    surface.with_pending_state(|s| s.size = Some(size.into()));
    state.lock.surfaces.retain(|e| e.idx != idx);
    state.lock.surfaces.push(Entry { idx, surface, size });
    server::mark_dirty_full(state);
}

// ─── Requests from the host (idle, lid, power menu, IPC) ──────────────────

/// Is there a lock client this build can start? Without one, covering the
/// screen would be a lockout.
pub fn available(state: &State) -> bool {
    state.supervisor.get("lock").is_some_and(|r| r.state != SupervisorState::Disabled)
}

/// Lock the session now. Returns false when it cannot (no lock client);
/// callers decide what that means for them.
pub fn request(state: &mut State) -> bool {
    if state.lock.is_active() {
        return true;
    }
    if !available(state) {
        tracing::error!("session-lock: no lock client is available; not locking");
        return false;
    }
    tracing::info!("session-lock: covering the session");
    close_modals(state);
    let now = Instant::now();
    state.lock.phase = Phase::Cover;
    state.lock.cover_since = Some(now);
    state.lock.last_spawn = None;
    state.lock.lock_res = None;
    state.lock.surfaces.clear();
    set_focus(state, None);
    server::mark_dirty_full(state);
    tick(state, now);
    true
}

/// Everything that could be holding the keyboard or showing on top.
fn close_modals(state: &mut State) {
    state.launcher = None;
    state.power_menu = None;
    state.wallpaper_chooser = None;
    state.show_help = false;
    for name in ["launch", "power", "settings"] {
        let _ = state.supervisor.stop_component(name);
    }
}

fn set_focus(state: &mut State, target: Option<WlSurface>) {
    let serial = state.next_serial();
    let kb = state.keyboard.clone();
    kb.set_focus(state, target, serial);
}

fn focus_surface(state: &State) -> Option<WlSurface> {
    let want = server::active_monitor_idx(state);
    let live = |e: &&Entry| e.surface.alive();
    state
        .lock
        .surfaces
        .iter()
        .filter(live)
        .find(|e| e.idx == want)
        .or_else(|| state.lock.surfaces.iter().find(|e| e.surface.alive()))
        .map(|e| e.surface.wl_surface().clone())
}

// ─── Policy (pure, tested) ───────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum Spawn {
    No,
    Start,
    Restart,
}

/// What to do about the lock client, given the supervisor's view of it.
pub fn spawn_decision(needs_client: bool, comp: Option<&SupervisorState>, since_last: Option<Duration>) -> Spawn {
    if !needs_client {
        return Spawn::No;
    }
    match comp {
        None | Some(SupervisorState::Disabled) => Spawn::No,
        // On its way, or about to be: the supervisor owns it.
        Some(SupervisorState::Starting { .. } | SupervisorState::Running { .. } | SupervisorState::Backoff { .. }) => {
            Spawn::No
        }
        Some(SupervisorState::Stopped) => {
            if since_last.is_none_or(|d| d >= SPAWN_GAP) {
                Spawn::Start
            } else {
                Spawn::No
            }
        }
        Some(SupervisorState::Failed { .. }) => {
            if since_last.is_none_or(|d| d >= FAILED_RETRY) {
                Spawn::Restart
            } else {
                Spawn::No
            }
        }
    }
}

/// Lock + suspend: is it time to suspend?
pub fn suspend_due(locked: bool, cover_age: Option<Duration>) -> bool {
    locked || cover_age.is_some_and(|a| a >= COVER_SUSPEND_GRACE)
}

// ─── Periodic work ───────────────────────────────────────────────────────

pub fn tick(state: &mut State, now: Instant) {
    if !state.lock.is_active() {
        return;
    }

    // A client that died before the lock was confirmed leaves a stale
    // `Locking`: back to the cover, and let a new client try.
    if matches!(state.lock.phase, Phase::Locking(_)) && !state.lock.client_alive() {
        tracing::warn!("session-lock: the lock client went away before locking; covering");
        state.lock.phase = Phase::Cover;
        state.lock.cover_since = Some(now);
        state.lock.lock_res = None;
        state.lock.surfaces.clear();
        reset_manager(state);
        server::mark_dirty_full(state);
    }

    // Keep a lock client running for as long as the session is locked.
    let needs_client = !state.lock.client_alive() && !matches!(state.lock.phase, Phase::Locking(_));
    let comp = state.supervisor.get("lock").map(|r| r.state.clone());
    let since = state.lock.last_spawn.map(|t| now.saturating_duration_since(t));
    match spawn_decision(needs_client, comp.as_ref(), since) {
        Spawn::No => {}
        Spawn::Start => {
            state.lock.last_spawn = Some(now);
            if let Err(e) = state.supervisor.start_component("lock", now) {
                tracing::warn!("session-lock: could not start the lock client: {e}");
            }
        }
        Spawn::Restart => {
            state.lock.last_spawn = Some(now);
            tracing::warn!("session-lock: the lock client crash-looped; restarting it");
            if let Err(e) = state.supervisor.restart_component("lock", now) {
                tracing::warn!("session-lock: could not restart the lock client: {e}");
            }
        }
    }

    // Follow monitor size changes; drop dead surfaces.
    state.lock.surfaces.retain(|e| e.surface.alive());
    let mut resized = false;
    for e in state.lock.surfaces.iter_mut() {
        let Some(m) = state.monitors.get(e.idx) else { continue };
        let size = (m.output_w, m.output_h);
        if e.size != size {
            e.surface.with_pending_state(|s| s.size = Some(size.into()));
            e.surface.send_configure();
            e.size = size;
            resized = true;
        }
    }
    if resized {
        server::mark_dirty_full(state);
    }

    // The keyboard belongs to a lock surface, or to nobody.
    let want = focus_surface(state);
    if state.keyboard.current_focus() != want {
        set_focus(state, want);
    }

    if state.lock.suspend_when_locked {
        let age = state.lock.cover_since.map(|t| now.saturating_duration_since(t));
        if suspend_due(state.lock.is_locked(), age) {
            state.lock.suspend_when_locked = false;
            state.request_suspend();
        }
    }
}

// ─── Compositing ─────────────────────────────────────────────────────────

/// Paint one monitor while the session is locked: the dimmed wallpaper, then
/// the lock surface if there is one. Nothing else is ever drawn.
pub fn paint(state: &mut State, idx: usize, w: u32, h: u32, clip: Rect) {
    let client_alive = state.lock.client_alive();
    let State { monitors, surface_buffers, text, background, lock, .. } = state;
    let m = &mut monitors[idx];
    server::fill_background(&mut m.composite_buf, m.wallpaper.as_deref(), *background, w, clip);
    server::blend_rect(&mut m.composite_buf, w, clip, [0, 0, 0, 255], 0.6);

    let buf = lock.surface_for(idx).and_then(|s| surface_buffers.get(&s.wl_surface().id()));
    match buf {
        Some(b) if (b.w, b.h) == (w, h) => server::blit(&mut m.composite_buf, w, h, b, 0, 0, clip),
        _ if !client_alive => {
            let lines = [
                ("Session locked", 30.0, [235, 235, 235, 255]),
                ("The lock screen is not running. Restarting it...", 18.0, [200, 200, 200, 255]),
                ("From a TTY: veil ctl restart lock", 16.0, [160, 160, 160, 255]),
            ];
            let mut y = h as f32 * 0.42;
            for (line, size, color) in lines {
                let (tw, th) = text.measure(line, Role::UI, size);
                text.draw_rgba(&mut m.composite_buf, w, h, line, (w as f32 - tw) / 2.0, y, Role::UI, size, color);
                y += th + 10.0;
            }
        }
        _ => {}
    }
}

/// After every monitor was composited this tick: once a `Locking` session has
/// shown a frame with no desktop in it, confirm the lock to the client.
pub fn after_composite(state: &mut State) {
    if !matches!(state.lock.phase, Phase::Locking(_)) {
        return;
    }
    if let Phase::Locking(locker) = std::mem::replace(&mut state.lock.phase, Phase::Locked) {
        locker.lock();
        tracing::info!("session-lock: locked");
    }
}

/// Lock surfaces need frame callbacks like any other surface.
pub fn surfaces(state: &State) -> Vec<WlSurface> {
    state.lock.surfaces.iter().filter(|e| e.surface.alive()).map(|e| e.surface.wl_surface().clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> SupervisorState {
        let now = Instant::now();
        SupervisorState::Running { started_at: now, last_pong: now }
    }

    #[test]
    fn no_client_needed_means_no_spawn() {
        assert_eq!(spawn_decision(false, Some(&SupervisorState::Stopped), None), Spawn::No);
    }

    #[test]
    fn a_stopped_client_is_started_but_not_hammered() {
        let s = SupervisorState::Stopped;
        assert_eq!(spawn_decision(true, Some(&s), None), Spawn::Start);
        assert_eq!(spawn_decision(true, Some(&s), Some(Duration::from_millis(500))), Spawn::No);
        assert_eq!(spawn_decision(true, Some(&s), Some(SPAWN_GAP)), Spawn::Start);
    }

    #[test]
    fn a_client_the_supervisor_is_handling_is_left_alone() {
        let starting = SupervisorState::Starting { spawned_at: Instant::now() };
        let backoff = SupervisorState::Backoff { attempt: 1, until: Instant::now() };
        for s in [starting, running(), backoff] {
            assert_eq!(spawn_decision(true, Some(&s), None), Spawn::No);
        }
    }

    #[test]
    fn a_crash_looped_client_is_retried_slowly_never_abandoned() {
        let s = SupervisorState::Failed { reason: "x".into() };
        assert_eq!(spawn_decision(true, Some(&s), None), Spawn::Restart);
        assert_eq!(spawn_decision(true, Some(&s), Some(Duration::from_secs(10))), Spawn::No);
        assert_eq!(spawn_decision(true, Some(&s), Some(FAILED_RETRY)), Spawn::Restart);
    }

    #[test]
    fn a_missing_or_disabled_client_is_never_spawned() {
        assert_eq!(spawn_decision(true, None, None), Spawn::No);
        assert_eq!(spawn_decision(true, Some(&SupervisorState::Disabled), None), Spawn::No);
    }

    #[test]
    fn suspend_waits_for_the_lock_or_a_covered_grace_period() {
        assert!(suspend_due(true, None));
        assert!(!suspend_due(false, None));
        assert!(!suspend_due(false, Some(Duration::from_secs(1))));
        assert!(suspend_due(false, Some(COVER_SUSPEND_GRACE)));
    }
}
