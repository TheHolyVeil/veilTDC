//! The Wayland side of the lock screen: take the session lock, put one lock
//! surface on every output, feed keys to the screen, unlock on success.
//!
//! Nothing here is a fallback. If the compositor refuses the lock
//! (`finished`), we exit; a client that exits without unlocking leaves the
//! session locked, and the compositor respawns it.

use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Surface},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers, RepeatInfo},
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface, SessionLockSurfaceConfigure,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use veil_ui::modal::{Key, Repeater};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};
use zeroize::Zeroizing;

use crate::auth::{self, Outcome};
use crate::screen::{Action, LockScreen};

/// Used until the compositor sends its own `repeat_info`.
const DEFAULT_REPEAT: RepeatInfo =
    RepeatInfo::Repeat { rate: std::num::NonZeroU32::new(25).unwrap(), delay: 400 };

struct Output {
    wl: wl_output::WlOutput,
    surface: SessionLockSurface,
    pool: Option<SlotPool>,
    w: u32,
    h: u32,
    configured: bool,
}

struct State {
    registry: RegistryState,
    compositor: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    lock_state: SessionLockState,
    lock: Option<SessionLock>,
    outputs: Vec<Output>,
    /// The compositor confirmed the lock.
    locked: bool,
    /// The compositor refused or ended the lock.
    finished: bool,
    mods: Modifiers,
    repeater: Repeater,
    keys: Vec<Key>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
}

impl State {
    /// A lock surface for `output`, if the lock exists and it has none yet.
    fn add_output(&mut self, qh: &QueueHandle<Self>, wl: &wl_output::WlOutput) {
        let Some(lock) = &self.lock else { return };
        if self.outputs.iter().any(|o| o.wl == *wl) {
            return;
        }
        let Ok(surface) = Surface::new(&self.compositor, qh) else {
            tracing::warn!("lock: could not create a surface");
            return;
        };
        let surface = lock.create_lock_surface(surface, wl, qh);
        self.outputs.push(Output { wl: wl.clone(), surface, pool: None, w: 0, h: 0, configured: false });
    }

    fn paint(&mut self, screen: &mut LockScreen) {
        for o in self.outputs.iter_mut().filter(|o| o.configured && o.w > 0 && o.h > 0) {
            let pixels = screen.render(o.w, o.h);
            let need = (o.w * o.h * 4) as usize * 2;
            let pool = o.pool.get_or_insert_with(|| SlotPool::new(need, &self.shm).expect("shm pool"));
            let Ok((buffer, canvas)) =
                pool.create_buffer(o.w as i32, o.h as i32, (o.w * 4) as i32, wl_shm::Format::Argb8888)
            else {
                continue;
            };
            // tiny-skia gives premultiplied R,G,B,A; wl_shm Argb8888 is B,G,R,A in memory.
            let n = canvas.len().min(pixels.len());
            let (dst, _) = canvas[..n].as_chunks_mut::<4>();
            let (src, _) = pixels[..n].as_chunks::<4>();
            for (d, s) in dst.iter_mut().zip(src) {
                *d = [s[2], s[1], s[0], s[3]];
            }
            canvas[n..].fill(0);
            let wl = o.surface.wl_surface();
            buffer.attach_to(wl).ok();
            wl.damage_buffer(0, 0, o.w as i32, o.h as i32);
            wl.commit();
        }
    }
}

/// Take the lock and run the lock screen until it is unlocked.
pub fn run(
    mut screen: LockScreen,
    user: String,
    avatar_rx: Receiver<Option<tiny_skia::Pixmap>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();

    let mut state = State {
        registry: RegistryState::new(&globals),
        compositor: CompositorState::bind(&globals, &qh)?,
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm: Shm::bind(&globals, &qh)?,
        lock_state: SessionLockState::new(&globals, &qh),
        lock: None,
        outputs: Vec::new(),
        locked: false,
        finished: false,
        mods: Modifiers::default(),
        repeater: Repeater::new(DEFAULT_REPEAT),
        keys: Vec::new(),
        keyboard: None,
    };
    // Outputs and seats are announced on the first roundtrips.
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    state.lock = Some(state.lock_state.lock(&qh).map_err(|e| format!("the compositor has no session lock: {e}"))?);
    let outs: Vec<_> = state.output_state.outputs().collect();
    for wl in &outs {
        state.add_output(&qh, wl);
    }
    queue.roundtrip(&mut state)?;

    let mut auth_rx: Option<Receiver<Outcome>> = None;
    let mut avatar_rx = Some(avatar_rx);
    let mut dirty = true;

    loop {
        queue.flush()?;
        if state.finished {
            tracing::warn!("lock: the compositor ended the lock; exiting");
            return Ok(());
        }

        let now = Instant::now();
        let clock_due = Duration::from_millis(1000 - u64::from(wall_millis()));
        let wake = [
            Some(if screen.is_animating() { Duration::from_millis(50) } else { clock_due }),
            avatar_rx.is_some().then_some(Duration::from_millis(100)),
            state.repeater.next_due().map(|t| t.saturating_duration_since(now)),
        ]
        .into_iter()
        .flatten()
        .min();
        if let Some(guard) = queue.prepare_read() {
            if wait_readable(&queue, wake) {
                guard.read()?;
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(&mut state)?;

        let now = Instant::now();
        while let Some(key) = state.repeater.due(now) {
            state.keys.push(key);
        }
        for key in std::mem::take(&mut state.keys) {
            match screen.key(&key) {
                Action::None => {}
                Action::Redraw => dirty = true,
                Action::Submit(pw) => {
                    dirty = true;
                    if state.locked {
                        auth_rx = Some(spawn_auth(&user, pw));
                    } else {
                        screen.failed("Not locked yet, try again".into(), now);
                    }
                }
            }
        }

        if let Some(rx) = &auth_rx {
            match rx.try_recv() {
                Ok(Outcome::Granted) => {
                    tracing::info!("lock: authenticated, unlocking");
                    if let Some(lock) = &state.lock {
                        lock.unlock();
                    }
                    // The unlock request must reach the compositor before we exit.
                    queue.roundtrip(&mut state)?;
                    return Ok(());
                }
                Ok(Outcome::Denied(msg)) => {
                    screen.failed(msg, now);
                    auth_rx = None;
                    dirty = true;
                }
                Ok(Outcome::Error(msg)) => {
                    tracing::error!("lock: {msg}");
                    screen.failed(msg, now);
                    auth_rx = None;
                    dirty = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    screen.failed("Authentication failed".into(), now);
                    auth_rx = None;
                    dirty = true;
                }
            }
        }

        // The profile picture arrives from its own thread.
        if let Some(rx) = &avatar_rx {
            match rx.try_recv() {
                Ok(found) => {
                    if let Some(pm) = found {
                        screen.set_avatar(pm);
                        dirty = true;
                    }
                    avatar_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => avatar_rx = None,
            }
        }

        if screen.tick(now) {
            dirty = true;
        }
        // The clock ticks once a second.
        if dirty || !screen.is_animating() {
            state.paint(&mut screen);
            dirty = false;
        }
    }
}

fn spawn_auth(user: &str, password: Zeroizing<String>) -> Receiver<Outcome> {
    let (tx, rx) = channel();
    let user = user.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(auth::authenticate(&user, password));
    });
    rx
}

fn wall_millis() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_millis()).unwrap_or(0)
}

/// Wait for the Wayland socket to become readable; `None` waits forever.
fn wait_readable(queue: &EventQueue<State>, timeout: Option<Duration>) -> bool {
    use rustix::event::{poll, PollFd, PollFlags};
    use rustix::fd::AsFd;
    use rustix::time::Timespec;

    let fd = queue.as_fd();
    let mut pfd = [PollFd::new(&fd, PollFlags::IN)];
    let ts = timeout.map(|d| Timespec { tv_sec: d.as_secs() as _, tv_nsec: d.subsec_nanos() as _ });
    loop {
        match poll(&mut pfd, ts.as_ref()) {
            Ok(n) => return n > 0,
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return false,
        }
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────

impl SessionLockHandler for State {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        tracing::info!("lock: session locked");
        self.locked = true;
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.finished = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        if let Some(o) = self.outputs.iter_mut().find(|o| o.surface.wl_surface() == surface.wl_surface()) {
            let (w, h) = configure.new_size;
            if (w, h) != (o.w, o.h) {
                o.pool = None;
            }
            o.w = w;
            o.h = h;
            o.configured = true;
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_output(qh, &output);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.outputs.retain(|o| o.wl != output);
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Keyboard {
            self.keyboard = None;
            self.repeater.clear();
        }
    }
}

impl KeyboardHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {
        self.repeater.clear();
    }

    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        let key = Key {
            sym: event.keysym,
            text: event.utf8.clone().filter(|s| !s.is_empty() && !s.chars().any(char::is_control)),
            ctrl: self.mods.ctrl,
            alt: self.mods.alt,
            shift: self.mods.shift,
            repeat: false,
        };
        self.repeater.press(event.raw_code, &key, Instant::now());
        self.keys.push(key);
    }

    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.repeater.release(event.raw_code);
    }

    /// Only sent when SCTK drives repeat through calloop, which this loop does
    /// not use: `Repeater` does it.
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}

    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, mods: Modifiers, _: RawModifiers, _: u32) {
        self.mods = mods;
    }

    fn update_repeat_info(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, info: RepeatInfo) {
        self.repeater.set_info(info);
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(State);
delegate_registry!(State);
