//! The notify process: the D-Bus service, the Veil IPC client, one layer
//! surface per toast, and the single loop that ties them to [`Core`].
//!
//! The loop sleeps in `poll(2)` on a waker fd (poked by the D-Bus and IPC
//! threads) plus every toast's Wayland socket, with a timeout from
//! [`Core::next_wake`]. Idle means no wakeups at all. A pass that changed
//! anything runs again before sleeping, because a request queued on a Wayland
//! connection is not sent until it is flushed and a pointer event may already
//! be buffered; sleeping on a stale socket would lose both.
//!
//! One surface per toast, anchored top-right on the compositor's usable area
//! (so below the bar), margin = its place in the stack. A new connection per
//! toast is deliberate: each has its own pointer events, so a click needs no
//! hit-testing across a tall shared surface.

use std::collections::{BTreeMap, HashSet};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use veil_ipc::notify::{Control, PanelMode};
use veil_ui::{Anchor, FontConfig, KeyboardInteractivity, Layer, LayerOptions, LayerShellClient, OutputFilter, TextSystem, Theme, UiInputEvent};

use crate::dbus::{Event, Service, ServiceConfig};
use crate::engine::{Core, CoreConfig, Effect};
use crate::ids::IdGen;
use crate::ipc::{Incoming, Ipc, IpcConfig};
use crate::layout::{self, StackConfig};
use crate::model::Notification;
use crate::panel::{self, PanelHit, PanelLayout, PanelStyle};
use crate::store::Store;
use crate::toast::{self, ToastHit, ToastLayout, ToastStyle};
use crate::waker::{poll_readable, Waker};

/// Set by the SIGTERM/SIGINT handler; the loop finishes cleanly when it sees it.
static TERMINATED: AtomicBool = AtomicBool::new(false);
/// The waker fd the signal handler pokes, so the loop wakes at once.
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    TERMINATED.store(true, Ordering::Relaxed);
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let one: u64 = 1;
        // SAFETY: write(2) is async-signal-safe; the buffer is a live u64.
        let _ = unsafe { libc::write(fd, (&one as *const u64).cast(), 8) };
    }
}

/// After a failed connection to the compositor, wait this long before trying again.
const WAYLAND_RETRY: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct Config {
    pub engine: CoreConfig,
    pub toast: ToastStyle,
    pub stack: StackConfig,
    pub panel: PanelStyle,
    /// Which monitor shows toasts. `First` until the compositor tells us which one has focus.
    pub output: OutputFilter,
    pub history_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            engine: CoreConfig::default(),
            toast: ToastStyle::default(),
            stack: StackConfig::default(),
            panel: PanelStyle::default(),
            output: OutputFilter::First,
            history_path: default_history_path(),
        }
    }
}

/// `$XDG_RUNTIME_DIR/veil/notify-history.json`, so it survives a crash and not a logout.
pub fn default_history_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("veil").join("notify-history.json"),
        None => {
            // SAFETY: geteuid has no preconditions and cannot fail.
            let uid = unsafe { libc::geteuid() };
            std::env::temp_dir().join(format!("veil-notify-{uid}")).join("notify-history.json")
        }
    }
}

/// Where to connect instead of the environment's defaults (tests).
#[derive(Default, Clone)]
pub struct Overrides {
    pub bus_address: Option<String>,
    pub ipc_socket: Option<PathBuf>,
    pub wayland_socket: Option<PathBuf>,
}

/// Lets another thread ask the loop to finish.
#[derive(Clone)]
pub struct Handle {
    stop: Arc<AtomicBool>,
    waker: Arc<Waker>,
}

impl Handle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.waker.wake();
    }

    /// The fd a signal handler may write to in order to wake the loop.
    pub fn waker_fd(&self) -> std::os::fd::RawFd {
        self.waker.fd()
    }
}

enum UiAction {
    Dismiss(u32),
    Activate(u32, String),
    Body(u32),
}

/// What the panel pointer events resolve to.
enum PanelAction {
    Close,
    Dnd,
    Clear,
    Remove(u32),
    /// Clicked a row body — no-op for now (no actions in history rows per spec).
    _Row(u32),
}

struct ToastSurface {
    client: LayerShellClient,
    note: Notification,
    layout: ToastLayout,
    top: i32,
    /// Got its first configure at the current size, so it can take a buffer.
    configured: bool,
    dirty: bool,
    pointer_inside: bool,
    hover: Option<ToastHit>,
}

/// The history panel: one persistent overlay surface, top-right, anchored below
/// the bar. Created on `Effect::Panel(true)`, destroyed on `Effect::Panel(false)`.
struct PanelSurface {
    client: LayerShellClient,
    layout: PanelLayout,
    /// Scroll offset in pixels (clamped inside `panel::layout`).
    scroll: i32,
    /// `history_rev` at the time of the last draw; redraw when it changes.
    history_rev: u64,
    configured: bool,
    dirty: bool,
    hover: Option<PanelHit>,
}

pub struct App {
    cfg: Config,
    overrides: Overrides,
    waker: Arc<Waker>,
    stop: Arc<AtomicBool>,
    service: Service,
    ipc: Ipc,
    engine: Core,
    text: TextSystem,
    theme: Theme,
    surfaces: BTreeMap<u32, ToastSurface>,
    hovering: bool,
    wayland_retry_at: Option<Instant>,
    relayout_all: bool,
    /// The history panel surface; `None` when the panel is closed.
    panel: Option<PanelSurface>,
}

fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl App {
    pub fn new(cfg: Config, overrides: Overrides) -> Result<(App, Handle), String> {
        let waker = Arc::new(Waker::new().map_err(|e| format!("eventfd: {e}"))?);
        let ids = IdGen::new(1);
        let loaded = Store::load(&cfg.history_path, &cfg.engine.limits);
        ids.raise_to(loaded.next_id);

        let service_cfg = ServiceConfig {
            limits: cfg.engine.limits.clone(),
            bus_address: overrides.bus_address.clone(),
            waker: Some(waker.clone()),
            ..ServiceConfig::default()
        };
        let service = Service::spawn(service_cfg, ids.clone()).map_err(|e| format!("D-Bus thread: {e}"))?;

        let mut ipc_cfg = IpcConfig::default();
        if let Some(p) = &overrides.ipc_socket {
            ipc_cfg.socket = p.clone();
        }
        ipc_cfg.waker = Some(waker.clone());
        let ipc = Ipc::spawn(ipc_cfg).map_err(|e| format!("IPC thread: {e}"))?;

        let engine = Core::new(cfg.engine.clone(), ids, loaded.store, Instant::now());
        let stop = Arc::new(AtomicBool::new(false));
        let handle = Handle { stop: stop.clone(), waker: waker.clone() };
        let app = App {
            cfg,
            overrides,
            waker,
            stop,
            service,
            ipc,
            engine,
            text: TextSystem::new(&FontConfig::default()),
            theme: Theme::default(),
            surfaces: BTreeMap::new(),
            hovering: false,
            wayland_retry_at: None,
            relayout_all: false,
            panel: None,
        };
        Ok((app, handle))
    }

    pub fn run(mut self) -> Result<(), String> {
        while !self.stop.load(Ordering::Relaxed) && !TERMINATED.load(Ordering::Relaxed) {
            let now = Instant::now();
            let mut changed = self.pump_sources();
            self.engine.on_tick(now, wall_ms());
            changed |= self.run_effects();
            if let Some(Err(e)) = self.engine.save_if_due(now, &self.cfg.history_path) {
                tracing::warn!("notify: cannot save the history: {e}");
            }
            self.sync_surfaces(now);
            self.sync_panel();
            self.relayout_all = false; // consumed by both sync_surfaces and sync_panel above
            self.draw_dirty();
            self.draw_panel();
            // Flush what we just queued and read whatever is already waiting.
            changed |= self.pump_wayland(now);
            changed |= self.pump_panel_wayland(now);
            if changed {
                continue;
            }

            let timeout = self.engine.next_wake().map(|t| t.saturating_duration_since(Instant::now()));
            let mut fds = vec![self.waker.fd()];
            fds.extend(self.surfaces.values().map(|s| s.client.wayland_fd()));
            if let Some(p) = &self.panel {
                fds.push(p.client.wayland_fd());
            }
            let ready = poll_readable(&fds, timeout);
            if ready.first().copied().unwrap_or(false) {
                self.waker.drain();
            }
        }
        if let Err(e) = self.engine.save_now(&self.cfg.history_path) {
            tracing::warn!("notify: cannot save the history on exit: {e}");
        }
        Ok(())
    }

    // ----- the D-Bus and IPC threads

    fn pump_sources(&mut self) -> bool {
        let mut any = false;
        let now = Instant::now();
        while let Ok(ev) = self.service.events.try_recv() {
            any = true;
            match ev {
                Event::Notify { notification, replaced } => self.engine.on_notify(notification, replaced, now),
                Event::Close { id } => self.engine.on_close_request(id, now),
                Event::Name(state) => tracing::info!("notify: bus name {state:?}"),
                Event::Unavailable(why) => tracing::warn!("notify: no D-Bus, running without notifications: {why}"),
            }
        }
        while let Ok(ev) = self.ipc.incoming.try_recv() {
            any = true;
            match ev {
                Incoming::Control(c) => self.engine.on_control(c, now),
                Incoming::Connected { palette, config } => {
                    self.apply_palette(&palette);
                    self.apply_config(&config);
                }
                Incoming::Palette(p) => self.apply_palette(&p),
                Incoming::Config(c) => self.apply_config(&c),
                Incoming::Disconnected => tracing::debug!("notify: compositor IPC dropped, reconnecting"),
            }
        }
        any
    }

    fn apply_palette(&mut self, palette: &Value) {
        if palette.is_object() {
            let tokens = veil_palette::Tokens::from_hex_lookup(|k| palette.get(k).and_then(|x| x.as_str()));
            self.theme = Theme::from_palette(&tokens);
            for s in self.surfaces.values_mut() {
                s.dirty = true;
            }
            if let Some(p) = &mut self.panel {
                p.dirty = true;
            }
        }
    }

    fn apply_config(&mut self, config: &Value) {
        if let Some(font) = config.get("font") {
            if self.text.reload(&FontConfig::from_json(font)) {
                self.relayout_all = true;
            }
        }
    }

    /// Run what the engine says the outside world must be told.
    fn run_effects(&mut self) -> bool {
        let effects = self.engine.take_effects();
        let any = !effects.is_empty();
        for e in effects {
            match e {
                Effect::Closed { id, reason } => self.service.closed(id, reason),
                Effect::Action { id, key } => self.service.action(id, &key),
                Effect::Publish(state) => self.ipc.publish(state),
                Effect::Panel(open) => self.apply_panel_effect(open),
            }
        }
        any
    }

    /// Open or close the panel surface in response to an engine effect.
    fn apply_panel_effect(&mut self, open: bool) {
        if open {
            if self.panel.is_none() {
                self.open_panel();
            }
        } else {
            if self.panel.is_some() {
                self.panel = None;
                tracing::debug!("notify: history panel closed");
            }
        }
    }

    /// Create the panel `LayerShellClient` and park the initial `PanelSurface`.
    fn open_panel(&mut self) {
        // If Wayland is not available yet we cannot open the panel; the engine
        // already tracks `panel_open = true` and will re-emit when asked again.
        if self.wayland_retry_at.is_some_and(|t| t > Instant::now()) {
            tracing::warn!("notify: cannot open panel, compositor not reachable yet");
            return;
        }

        let st = &self.cfg.panel;
        // Build an initial layout so we know the height before the first configure.
        let rows: Vec<_> = self.engine.store().newest_first().map(panel::Row::from_notification).collect();
        let dnd_on = self.engine.store().dnd();
        let initial_layout = panel::layout(&rows, dnd_on, 0, wall_ms(), st, &mut self.text);

        let stream = match &self.overrides.wayland_socket {
            Some(path) => match UnixStream::connect(path) {
                Ok(s) => Some(s),
                Err(e) => {
                    self.wayland_failed(&e.to_string(), Instant::now());
                    return;
                }
            },
            None => None,
        };

        let opts = LayerOptions {
            namespace: "veil-notify-panel".into(),
            layer: Layer::Overlay,
            // Top-right, same quadrant as toasts but a separate surface.
            anchor: Anchor::TOP | Anchor::RIGHT,
            size: (initial_layout.width, initial_layout.height),
            exclusive_zone: 0,
            // The panel needs keyboard so the user can scroll and press keys.
            keyboard: KeyboardInteractivity::OnDemand,
            margin: (self.cfg.stack.margin_top as i32, self.cfg.stack.margin_right as i32, 0, 0),
            output: self.cfg.output.clone(),
        };

        match LayerShellClient::with_options(opts, stream) {
            Ok(client) => {
                self.wayland_retry_at = None;
                let history_rev = self.engine.history_rev();
                self.panel = Some(PanelSurface {
                    client,
                    layout: initial_layout,
                    scroll: 0,
                    history_rev,
                    configured: false,
                    dirty: true,
                    hover: None,
                });
                tracing::debug!("notify: history panel opened");
            }
            Err(e) => self.wayland_failed(&e, Instant::now()),
        }
    }

    /// Rebuild the panel layout and mark it dirty whenever the history or DND
    /// changed, or after a font reload.
    fn sync_panel(&mut self) {
        let p = match &mut self.panel {
            Some(p) => p,
            None => return,
        };
        let rev = self.engine.history_rev();
        let force = self.relayout_all;
        if rev == p.history_rev && !force {
            return;
        }
        let st = &self.cfg.panel;
        let rows: Vec<_> = self.engine.store().newest_first().map(panel::Row::from_notification).collect();
        let dnd_on = self.engine.store().dnd();
        let new_layout = panel::layout(&rows, dnd_on, p.scroll, wall_ms(), st, &mut self.text);

        // If the size changed, tell the compositor; we'll get a new configure.
        if (new_layout.width, new_layout.height) != (p.layout.width, p.layout.height) {
            p.client.reconfigure(Anchor::TOP | Anchor::RIGHT, (new_layout.width, new_layout.height), 0);
            p.configured = false;
        }
        p.layout = new_layout;
        p.history_rev = rev;
        p.dirty = true;
    }

    /// Present the panel pixmap if it's dirty and has been configured.
    fn draw_panel(&mut self) {
        let p = match &mut self.panel {
            Some(p) if p.dirty && p.configured => p,
            _ => return,
        };
        let pm = panel::draw(&p.layout, &self.cfg.panel, &self.theme, &mut self.text, p.hover);
        let bytes = pm.take();
        let (w, h) = (p.layout.width, p.layout.height);
        p.client.present_for_each(|_, sw, sh| {
            if (sw, sh) == (w, h) { bytes.clone() } else { vec![0u8; (sw * sh * 4) as usize] }
        });
        p.dirty = false;
    }

    /// Flush the panel connection and dispatch pointer events into engine calls.
    /// Returns `true` if anything changed.
    fn pump_panel_wayland(&mut self, now: Instant) -> bool {
        // Phase 1: dispatch the connection and collect raw events; release the
        // `self.panel` borrow before we touch `self.engine` or `self.cfg`.
        {
            let p = match &mut self.panel {
                Some(p) => p,
                None => return false,
            };
            if let Err(e) = p.client.dispatch() {
                tracing::warn!("notify: lost the compositor for the panel: {e}");
                self.panel = None;
                return true;
            }
        }

        let mut changed = false;
        let p = self.panel.as_mut().unwrap();

        if p.client.take_reconfigured() {
            p.configured = true;
            p.dirty = true;
            changed = true;
        }

        // Drain all input events into typed queues so we can process them
        // without holding a borrow on `self.panel`.
        let mut pending_actions: Vec<PanelAction> = Vec::new();
        let mut scroll_delta: i32 = 0;

        while let Ok(ev) = p.client.input_rx.try_recv() {
            changed = true;
            match ev {
                UiInputEvent::Motion { x, y, .. } => {
                    let over = panel::hit(&p.layout, x as i32, y as i32, &self.cfg.panel);
                    if over != p.hover {
                        p.hover = over;
                        p.dirty = true;
                    }
                }
                UiInputEvent::Leave { .. } => {
                    if p.hover.take().is_some() {
                        p.dirty = true;
                    }
                }
                UiInputEvent::Press { x, y, .. } => {
                    if let Some(hit) = panel::hit(&p.layout, x as i32, y as i32, &self.cfg.panel) {
                        match hit {
                            PanelHit::Close => pending_actions.push(PanelAction::Close),
                            PanelHit::Dnd => pending_actions.push(PanelAction::Dnd),
                            PanelHit::Clear => pending_actions.push(PanelAction::Clear),
                            PanelHit::Remove(id) => pending_actions.push(PanelAction::Remove(id)),
                            PanelHit::Row(_id) => {} // history rows have no actions (spec: sender may be gone)
                        }
                    }
                }
                UiInputEvent::Scroll { dy, .. } => {
                    // Accumulate: one finger swipe can produce many events in one pass.
                    // 54 px per natural scroll unit ≈ one row height; dy > 0 → scroll down.
                    scroll_delta += (dy * 54.0) as i32;
                }
                UiInputEvent::PressRight { .. } => {}
            }
        }

        // Apply accumulated scroll, then rebuild the layout so hit-test is in
        // sync with what will be drawn.  The `p` borrow ends here before we call
        // back into `self`.
        if scroll_delta != 0 {
            let p = self.panel.as_mut().unwrap();
            let new_scroll = (p.scroll + scroll_delta).max(0);
            if new_scroll != p.scroll {
                p.scroll = new_scroll;
                p.dirty = true;
            }
            // Rebuild layout with the new scroll offset.
            let scroll = self.panel.as_ref().unwrap().scroll;
            let rows: Vec<_> = self.engine.store().newest_first().map(panel::Row::from_notification).collect();
            let dnd_on = self.engine.store().dnd();
            let new_layout = panel::layout(&rows, dnd_on, scroll, wall_ms(), &self.cfg.panel, &mut self.text);
            self.panel.as_mut().unwrap().layout = new_layout;
        }

        // Process click/button actions; each one may mutate the engine which
        // will show up in `history_rev` and cause `sync_panel` to redraw.
        for action in pending_actions {
            match action {
                PanelAction::Close => {
                    // Tell the engine to close the panel; it emits Effect::Panel(false).
                    self.engine.on_control(Control { dnd: None, panel: Some(PanelMode::Close) }, now);
                }
                PanelAction::Dnd => {
                    self.engine.on_control(Control { dnd: Some(veil_ipc::notify::DndMode::Toggle), panel: None }, now);
                }
                PanelAction::Clear => {
                    self.engine.clear_history(now);
                }
                PanelAction::Remove(id) => {
                    self.engine.remove_history(id, now);
                }
                PanelAction::_Row(_) => {}
            }
            changed = true;
        }

        changed
    }

    // ----- surfaces

    /// Make the set of surfaces match what the engine wants on screen.
    fn sync_surfaces(&mut self, now: Instant) {
        let want: Vec<Notification> = self.engine.visible().into_iter().cloned().collect();
        let want_ids: HashSet<u32> = want.iter().map(|n| n.id).collect();
        self.surfaces.retain(|id, _| want_ids.contains(id));

        let force = self.relayout_all;
        let layouts: Vec<ToastLayout> = want
            .iter()
            .map(|n| match self.surfaces.get(&n.id) {
                Some(s) if !force && s.note == *n => s.layout.clone(),
                _ => toast::layout(n, &self.cfg.toast, &mut self.text),
            })
            .collect();
        let placement = layout::place(&self.cfg.stack, &layouts.iter().map(|l| l.height).collect::<Vec<_>>());
        let right = self.cfg.stack.margin_right as i32;

        for (i, n) in want.iter().enumerate() {
            if i >= placement.shown() {
                self.surfaces.remove(&n.id);
                continue;
            }
            let (top, layout) = (placement.margins[i], &layouts[i]);
            match self.surfaces.get_mut(&n.id) {
                Some(s) => {
                    if s.note != *n || force {
                        let size_changed = (s.layout.width, s.layout.height) != (layout.width, layout.height);
                        s.note = n.clone();
                        s.layout = layout.clone();
                        s.dirty = true;
                        if size_changed {
                            s.client.reconfigure(Anchor::TOP | Anchor::RIGHT, (layout.width, layout.height), 0);
                            s.configured = false;
                        }
                    }
                    if s.top != top {
                        s.client.set_margin((top, right, 0, 0));
                        s.top = top;
                    }
                }
                None => self.create_surface(n, layout, top, now),
            }
        }
    }

    fn create_surface(&mut self, n: &Notification, layout: &ToastLayout, top: i32, now: Instant) {
        if self.wayland_retry_at.is_some_and(|t| t > now) {
            return;
        }
        let stream = match &self.overrides.wayland_socket {
            Some(path) => match UnixStream::connect(path) {
                Ok(s) => Some(s),
                Err(e) => return self.wayland_failed(&e.to_string(), now),
            },
            None => None,
        };
        let opts = LayerOptions {
            namespace: "veil-notify".into(),
            layer: Layer::Overlay,
            anchor: Anchor::TOP | Anchor::RIGHT,
            size: (layout.width, layout.height),
            exclusive_zone: 0,
            keyboard: KeyboardInteractivity::None,
            margin: (top, self.cfg.stack.margin_right as i32, 0, 0),
            output: self.cfg.output.clone(),
        };
        match LayerShellClient::with_options(opts, stream) {
            Ok(client) => {
                self.wayland_retry_at = None;
                self.surfaces.insert(
                    n.id,
                    ToastSurface { client, note: n.clone(), layout: layout.clone(), top, configured: false, dirty: true, pointer_inside: false, hover: None },
                );
            }
            Err(e) => self.wayland_failed(&e, now),
        }
    }

    fn wayland_failed(&mut self, why: &str, now: Instant) {
        tracing::warn!("notify: cannot show toasts, no compositor ({why}); retrying in {WAYLAND_RETRY:?}");
        self.wayland_retry_at = Some(now + WAYLAND_RETRY);
    }

    fn draw_dirty(&mut self) {
        for s in self.surfaces.values_mut().filter(|s| s.dirty && s.configured) {
            let pixmap = toast::draw(&s.note, &s.layout, &self.cfg.toast, &self.theme, &mut self.text, s.hover);
            let bytes = pixmap.take();
            let (w, h) = (s.layout.width, s.layout.height);
            s.client.present_for_each(|_, sw, sh| if (sw, sh) == (w, h) { bytes.clone() } else { vec![0; (sw * sh * 4) as usize] });
            s.dirty = false;
        }
    }

    /// Flush requests, read events, turn pointer input into engine calls.
    /// Returns whether anything happened that needs another pass.
    fn pump_wayland(&mut self, now: Instant) -> bool {
        let mut changed = false;
        let mut actions = Vec::new();
        let mut dead = Vec::new();
        for (&id, s) in self.surfaces.iter_mut() {
            if let Err(e) = s.client.dispatch() {
                tracing::warn!("notify: lost the compositor for toast {id}: {e}");
                dead.push(id);
                continue;
            }
            if s.client.take_reconfigured() {
                s.configured = true;
                s.dirty = true;
                changed = true;
            }
            while let Ok(ev) = s.client.input_rx.try_recv() {
                changed = true;
                match ev {
                    UiInputEvent::Motion { x, y, .. } => {
                        s.pointer_inside = true;
                        let over = toast::hit(&s.layout, x as i32, y as i32);
                        if over != s.hover {
                            s.hover = over;
                            s.dirty = true;
                        }
                    }
                    UiInputEvent::Leave { .. } => {
                        s.pointer_inside = false;
                        if s.hover.take().is_some() {
                            s.dirty = true;
                        }
                    }
                    UiInputEvent::Press { x, y, .. } => match toast::hit(&s.layout, x as i32, y as i32) {
                        Some(ToastHit::Close) => actions.push(UiAction::Dismiss(id)),
                        Some(ToastHit::Button(i)) => actions.push(UiAction::Activate(id, s.layout.buttons[i].key.clone())),
                        Some(ToastHit::Body) => actions.push(UiAction::Body(id)),
                        None => {}
                    },
                    UiInputEvent::PressRight { .. } => actions.push(UiAction::Dismiss(id)),
                    UiInputEvent::Scroll { .. } => {}
                }
            }
        }
        for id in dead {
            self.surfaces.remove(&id);
        }
        for a in actions {
            match a {
                UiAction::Dismiss(id) => self.engine.dismiss(id, now),
                UiAction::Activate(id, key) => self.engine.activate(id, &key, now),
                UiAction::Body(id) => self.engine.body_click(id, now),
            }
        }
        let inside = self.surfaces.values().any(|s| s.pointer_inside);
        if inside != self.hovering {
            self.hovering = inside;
            self.engine.hover(inside, now);
            changed = true;
        }
        changed
    }
}

/// `veil shell notify`: run until SIGTERM or SIGINT.
pub fn run_notify() -> Result<(), String> {
    // Reached through the multi-call binary, so the subscriber is set up here.
    // stderr, because the session log only captures stderr.
    let filter = std::env::var("VEIL_LOG")
        .ok()
        .and_then(|f| tracing_subscriber::EnvFilter::try_new(f).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(filter).try_init();
    tracing::info!("Starting veil-notify...");

    let (app, handle) = App::new(Config::default(), Overrides::default())?;
    SIGNAL_FD.store(handle.waker_fd(), Ordering::Relaxed);
    // SAFETY: `on_signal` only touches atomics and calls write(2); both are async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_history_lives_in_the_runtime_dir() {
        // Read-only checks: this test must not change the process environment.
        let p = default_history_path();
        assert!(p.ends_with("notify-history.json"));
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            assert_eq!(p, PathBuf::from(rt).join("veil").join("notify-history.json"));
        }
    }

    #[test]
    fn defaults_show_toasts_on_one_output_only() {
        assert_eq!(Config::default().output, OutputFilter::First);
    }
}
