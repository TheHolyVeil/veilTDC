//! A modal overlay for the shell components that take over the screen
//! (power menu, launcher): one exclusive-keyboard layer surface on the active
//! output, pointer and keyboard input with working key repeat, optional
//! `ext-background-effect-v1` blur, and an event loop that sleeps until
//! something happens.
//!
//! The component supplies a [`ModalApp`]; everything Wayland lives here, once.
//! It does no drawing of its own: `render` returns premultiplied RGBA for the
//! surface size.

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers, RepeatInfo},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};

/// BTN_LEFT from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;
/// Used until the compositor sends its own `repeat_info`.
const DEFAULT_REPEAT: RepeatInfo = RepeatInfo::Repeat { rate: NonZeroU32::new(25).unwrap(), delay: 400 };

/// One key press (or auto-repeat of one) delivered to the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub sym: Keysym,
    /// The text it types, when it types printable text (never control
    /// characters: Enter, Escape and Backspace are matched on `sym`).
    pub text: Option<String>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// True for the auto-repeats after the first press.
    pub repeat: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pointer {
    Move { x: f32, y: f32 },
    /// Left button released where it was pressed.
    Click { x: f32, y: f32 },
    /// Vertical scroll in pixels, positive down.
    Scroll { dy: f32 },
    Leave,
}

/// What the app wants after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flow {
    #[default]
    Idle,
    Redraw,
    Close,
}

pub trait ModalApp {
    /// Premultiplied RGBA8, exactly `w * h * 4` bytes.
    fn render(&mut self, w: u32, h: u32) -> Vec<u8>;
    fn key(&mut self, key: &Key) -> Flow;
    fn pointer(&mut self, _ev: Pointer) -> Flow {
        Flow::Idle
    }
    /// Rectangles `(x, y, w, h)` to blur behind, when the compositor offers
    /// it. Default: the whole surface.
    fn blur(&self, w: u32, h: u32) -> Vec<(i32, i32, i32, i32)> {
        vec![(0, 0, w as i32, h as i32)]
    }
    /// When the app next wants `tick` (a cursor blink). `None` sleeps until
    /// input arrives.
    fn next_tick(&self, _now: Instant) -> Option<Duration> {
        None
    }
    fn tick(&mut self, _now: Instant) -> Flow {
        Flow::Idle
    }
}

pub struct Options {
    /// Layer-shell namespace; the compositor trusts overlay clients by it.
    pub namespace: &'static str,
}

/// Show the modal and run until it closes.
pub fn run(app: Box<dyn ModalApp>, opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let bg_manager = globals.bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, ()).ok();
    tracing::debug!("{}: background blur {}", opts.namespace, if bg_manager.is_some() { "available" } else { "not offered" });

    let mut state = State {
        registry: RegistryState::new(&globals),
        compositor,
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        layer_shell,
        bg_manager,
        bg_surface: None,
        surface: None,
        pool: None,
        w: 0,
        h: 0,
        configured: false,
        closing: false,
        dirty: true,
        mouse: (0.0, 0.0),
        pressed: false,
        pointer: None,
        mods: Modifiers::default(),
        repeater: Repeater::new(DEFAULT_REPEAT),
        app,
    };

    // No output named: the compositor puts the surface where the pointer is,
    // which is the monitor you are looking at.
    state.open_surface(&qh, opts.namespace);
    queue.roundtrip(&mut state)?;

    while !state.closing {
        queue.flush()?;
        let now = Instant::now();
        let app_tick = state.app.next_tick(now).map(|d| now + d);
        let wake = [state.repeater.next_due(), app_tick].into_iter().flatten().min();

        if let Some(guard) = queue.prepare_read() {
            if wait_readable(&queue, wake.map(|t| t.saturating_duration_since(now))) {
                guard.read()?;
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(&mut state)?;

        let now = Instant::now();
        while let Some(key) = state.repeater.due(now) {
            state.on_key(key);
        }
        if app_tick.is_some_and(|t| now >= t) {
            let f = state.app.tick(now);
            state.apply(f);
        }
        if state.closing {
            break;
        }
        if state.configured && state.dirty {
            state.dirty = false;
            state.repaint()?;
        }
    }
    Ok(())
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

// ----------------------------------------------------------------- key repeat

/// Auto-repeat for one held key. The compositor only sends press and release;
/// repeating is the client's job (and SCTK only does it with calloop).
struct Repeater {
    info: RepeatInfo,
    held: Option<(u32, Key, Instant)>,
}

impl Repeater {
    fn new(info: RepeatInfo) -> Self {
        Repeater { info, held: None }
    }

    fn set_info(&mut self, info: RepeatInfo) {
        self.info = info;
        if matches!(info, RepeatInfo::Disable) {
            self.held = None;
        }
    }

    /// A key went down. Modifiers, Enter, Escape and Tab never repeat: holding
    /// Enter must not run an action over and over.
    fn press(&mut self, raw: u32, key: &Key, now: Instant) {
        let RepeatInfo::Repeat { delay, .. } = self.info else {
            self.held = None;
            return;
        };
        let no_repeat = key.sym.is_modifier_key()
            || matches!(key.sym, Keysym::Return | Keysym::KP_Enter | Keysym::Escape | Keysym::Tab | Keysym::ISO_Left_Tab);
        self.held = (!no_repeat).then(|| (raw, key.clone(), now + Duration::from_millis(u64::from(delay))));
    }

    fn release(&mut self, raw: u32) {
        if self.held.as_ref().is_some_and(|(r, ..)| *r == raw) {
            self.held = None;
        }
    }

    fn clear(&mut self) {
        self.held = None;
    }

    fn next_due(&self) -> Option<Instant> {
        self.held.as_ref().map(|(_, _, t)| *t)
    }

    /// The next repeat that is due at `now`, if any. Call until `None`: after a
    /// stall several may be owed, but never more than one per interval.
    fn due(&mut self, now: Instant) -> Option<Key> {
        let RepeatInfo::Repeat { rate, .. } = self.info else { return None };
        let (_, key, next) = self.held.as_mut()?;
        if now < *next {
            return None;
        }
        let interval = Duration::from_micros(1_000_000 / u64::from(rate.get()));
        // Do not replay a long stall as a burst: resume from now.
        *next = if now.saturating_duration_since(*next) > interval * 3 { now + interval } else { *next + interval };
        let mut k = key.clone();
        k.repeat = true;
        Some(k)
    }
}

// ------------------------------------------------------------- Wayland state

struct State {
    registry: RegistryState,
    compositor: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    layer_shell: LayerShell,
    bg_manager: Option<ExtBackgroundEffectManagerV1>,
    bg_surface: Option<ExtBackgroundEffectSurfaceV1>,
    surface: Option<LayerSurface>,
    pool: Option<SlotPool>,
    w: u32,
    h: u32,
    configured: bool,
    closing: bool,
    dirty: bool,
    mouse: (f32, f32),
    pressed: bool,
    pointer: Option<ThemedPointer>,
    mods: Modifiers,
    repeater: Repeater,
    app: Box<dyn ModalApp>,
}

impl State {
    fn open_surface(&mut self, qh: &QueueHandle<Self>, namespace: &str) {
        let surface = self.compositor.create_surface(qh);
        let ls = self.layer_shell.create_layer_surface(qh, surface, Layer::Overlay, Some(namespace), None);
        ls.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        ls.set_exclusive_zone(-1);
        ls.set_size(0, 0);
        ls.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        if let Some(mgr) = &self.bg_manager {
            self.bg_surface = Some(mgr.get_background_effect(ls.wl_surface(), qh, ()));
        }
        ls.commit();
        self.pool = SlotPool::new(1920 * 1080 * 4, &self.shm).ok();
        self.surface = Some(ls);
    }

    fn update_blur(&self, qh: &QueueHandle<Self>) {
        let Some(effect) = &self.bg_surface else { return };
        let rects = self.app.blur(self.w, self.h);
        if rects.is_empty() {
            effect.set_blur_region(None);
            return;
        }
        let region = self.compositor.wl_compositor().create_region(qh, ());
        for (x, y, w, h) in rects {
            region.add(x, y, w, h);
        }
        effect.set_blur_region(Some(&region));
        region.destroy();
    }

    fn apply(&mut self, flow: Flow) {
        match flow {
            Flow::Idle => {}
            Flow::Redraw => self.dirty = true,
            Flow::Close => self.closing = true,
        }
    }

    fn on_key(&mut self, key: Key) {
        let f = self.app.key(&key);
        self.apply(f);
    }

    fn make_key(&self, event: &KeyEvent) -> Key {
        Key {
            sym: event.keysym,
            text: event.utf8.clone().filter(|s| !s.is_empty() && !s.chars().any(char::is_control)),
            ctrl: self.mods.ctrl,
            alt: self.mods.alt,
            shift: self.mods.shift,
            repeat: false,
        }
    }

    fn repaint(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.w == 0 || self.h == 0 {
            return Ok(());
        }
        let (Some(ls), Some(pool)) = (&self.surface, &mut self.pool) else { return Ok(()) };
        let pixels = self.app.render(self.w, self.h);
        let (buffer, canvas) = pool.create_buffer(self.w as i32, self.h as i32, (self.w * 4) as i32, wl_shm::Format::Argb8888)?;
        let n = canvas.len().min(pixels.len());
        canvas[..n].copy_from_slice(&pixels[..n]);
        canvas[n..].fill(0);
        let wl = ls.wl_surface();
        buffer.attach_to(wl).ok();
        wl.damage_buffer(0, 0, self.w as i32, self.h as i32);
        ls.commit();
        Ok(())
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer && self.pointer.is_none() {
            let cs = self.compositor.create_surface(qh);
            if let Ok(p) = self.seat_state.get_pointer_with_theme(qh, &seat, self.shm.wl_shm(), cs, ThemeSpec::System) {
                self.pointer = Some(p);
            }
        }
        if cap == Capability::Keyboard {
            let _ = self.seat_state.get_keyboard(qh, &seat, None);
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, cap: Capability) {
        if cap == Capability::Pointer {
            self.pointer = None;
        }
        if cap == Capability::Keyboard {
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
        let key = self.make_key(&event);
        self.repeater.press(event.raw_code, &key, Instant::now());
        self.on_key(key);
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

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for ev in events {
            let flow = match ev.kind {
                PointerEventKind::Motion { .. } | PointerEventKind::Enter { .. } => {
                    self.mouse = (ev.position.0 as f32, ev.position.1 as f32);
                    self.app.pointer(Pointer::Move { x: self.mouse.0, y: self.mouse.1 })
                }
                PointerEventKind::Leave { .. } => {
                    self.pressed = false;
                    self.app.pointer(Pointer::Leave)
                }
                PointerEventKind::Press { button: BTN_LEFT, .. } => {
                    self.pressed = true;
                    Flow::Idle
                }
                PointerEventKind::Release { button: BTN_LEFT, .. } if self.pressed => {
                    self.pressed = false;
                    self.app.pointer(Pointer::Click { x: self.mouse.0, y: self.mouse.1 })
                }
                PointerEventKind::Axis { vertical, .. } => {
                    let dy = if vertical.absolute != 0.0 { vertical.absolute } else { f64::from(vertical.discrete) * 15.0 };
                    if dy == 0.0 {
                        Flow::Idle
                    } else {
                        self.app.pointer(Pointer::Scroll { dy: dy as f32 })
                    }
                }
                _ => Flow::Idle,
            };
            self.apply(flow);
        }
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.closing = true;
    }

    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, ls: &LayerSurface, cfg: LayerSurfaceConfigure, _: u32) {
        if self.surface.as_ref().is_some_and(|s| s.wl_surface() == ls.wl_surface()) {
            if cfg.new_size.0 > 0 {
                self.w = cfg.new_size.0;
            }
            if cfg.new_size.1 > 0 {
                self.h = cfg.new_size.1;
            }
            self.configured = true;
            self.dirty = true;
            self.update_blur(qh);
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl wayland_client::Dispatch<wl_region::WlRegion, ()> for State {
    fn event(_: &mut Self, _: &wl_region::WlRegion, _: wl_region::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl wayland_client::Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ExtBackgroundEffectManagerV1, event: ext_background_effect_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        tracing::debug!("ext_background_effect_manager_v1: {event:?}");
    }
}

impl wayland_client::Dispatch<ExtBackgroundEffectSurfaceV1, ()> for State {
    fn event(_: &mut Self, _: &ExtBackgroundEffectSurfaceV1, _: ext_background_effect_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_compositor!(State);
smithay_client_toolkit::delegate_output!(State);
smithay_client_toolkit::delegate_seat!(State);
smithay_client_toolkit::delegate_pointer!(State);
smithay_client_toolkit::delegate_keyboard!(State);
smithay_client_toolkit::delegate_layer!(State);
smithay_client_toolkit::delegate_shm!(State);
smithay_client_toolkit::delegate_registry!(State);

#[cfg(test)]
mod tests {
    use super::*;

    fn key(sym: Keysym) -> Key {
        Key { sym, text: None, ctrl: false, alt: false, shift: false, repeat: false }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn info(rate: u32, delay: u32) -> RepeatInfo {
        RepeatInfo::Repeat { rate: NonZeroU32::new(rate).unwrap(), delay }
    }

    #[test]
    fn a_held_key_waits_the_delay_then_repeats_at_the_rate() {
        let t = Instant::now();
        let mut r = Repeater::new(info(20, 300)); // 50 ms interval
        r.press(30, &key(Keysym::BackSpace), t);
        assert_eq!(r.next_due(), Some(t + ms(300)));
        assert!(r.due(t + ms(299)).is_none());
        let k = r.due(t + ms(300)).unwrap();
        assert!(k.repeat && k.sym == Keysym::BackSpace);
        assert!(r.due(t + ms(300)).is_none(), "one per interval");
        assert!(r.due(t + ms(349)).is_none());
        assert!(r.due(t + ms(350)).is_some());
        assert_eq!(r.next_due(), Some(t + ms(400)));
    }

    #[test]
    fn release_stops_only_the_key_that_is_held() {
        let t = Instant::now();
        let mut r = Repeater::new(info(25, 400));
        r.press(30, &key(Keysym::Left), t);
        r.release(31); // some other key
        assert!(r.next_due().is_some());
        r.release(30);
        assert!(r.next_due().is_none());
        assert!(r.due(t + ms(5000)).is_none());
    }

    #[test]
    fn a_newer_key_replaces_the_held_one() {
        let t = Instant::now();
        let mut r = Repeater::new(info(25, 400));
        r.press(30, &key(Keysym::Left), t);
        r.press(31, &key(Keysym::Right), t + ms(100));
        let k = r.due(t + ms(600)).unwrap();
        assert_eq!(k.sym, Keysym::Right);
    }

    #[test]
    fn enter_escape_tab_and_modifiers_never_repeat() {
        let t = Instant::now();
        let mut r = Repeater::new(info(25, 400));
        for sym in [Keysym::Return, Keysym::KP_Enter, Keysym::Escape, Keysym::Tab, Keysym::Shift_L, Keysym::Control_R, Keysym::Alt_L] {
            r.press(1, &key(sym), t);
            assert!(r.next_due().is_none(), "{sym:?}");
        }
    }

    #[test]
    fn repeat_can_be_disabled_by_the_compositor() {
        let t = Instant::now();
        let mut r = Repeater::new(info(25, 400));
        r.press(30, &key(Keysym::a), t);
        r.set_info(RepeatInfo::Disable);
        assert!(r.next_due().is_none(), "disabling drops the held key");
        r.press(30, &key(Keysym::a), t);
        assert!(r.next_due().is_none());
        assert!(r.due(t + ms(5000)).is_none());
    }

    #[test]
    fn a_stall_is_not_replayed_as_a_burst() {
        let t = Instant::now();
        let mut r = Repeater::new(info(20, 300));
        r.press(30, &key(Keysym::Down), t);
        assert!(r.due(t + ms(10_000)).is_some());
        assert!(r.due(t + ms(10_000)).is_none(), "resumes from now, not 190 owed repeats");
        assert_eq!(r.next_due(), Some(t + ms(10_050)));
    }
}
